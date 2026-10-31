use std::collections::BTreeMap;

use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use router_core::{
    Capabilities, EventStream, ProviderAdapter, ProviderConfig, ProviderEvent, ProviderStream,
    RouterError,
};
use router_protocol::{ChatCompletionRequest, Message, MessageContent, Tool, Usage};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    attachment::prepare_pdf_fallback,
    sse::{request_error, upstream_request_id},
    transport::ProviderTransport,
};

pub struct AnthropicAdapter {
    transport: ProviderTransport,
    endpoint: String,
}

impl AnthropicAdapter {
    pub fn new(config: &ProviderConfig) -> Result<Self, RouterError> {
        Ok(Self {
            transport: ProviderTransport::new(config)?,
            endpoint: format!(
                "{}/messages",
                config.effective_base_url()?.trim_end_matches('/')
            ),
        })
    }
}

#[async_trait]
impl ProviderAdapter for AnthropicAdapter {
    async fn discover_models(
        &self,
        config: &ProviderConfig,
        api_key: Option<&SecretString>,
    ) -> Option<Vec<router_protocol::ModelObject>> {
        crate::models::discover(config, api_key).await
    }

    async fn stream(
        &self,
        model: &str,
        request: ChatCompletionRequest,
        capabilities: Capabilities,
        api_key: Option<&SecretString>,
        cancel: CancellationToken,
    ) -> Result<EventStream, RouterError> {
        Ok(self
            .open_stream(model, request, capabilities, api_key, cancel)
            .await?
            .events)
    }

    async fn open_stream(
        &self,
        model: &str,
        mut request: ChatCompletionRequest,
        capabilities: Capabilities,
        api_key: Option<&SecretString>,
        cancel: CancellationToken,
    ) -> Result<ProviderStream, RouterError> {
        validate_nested_extensions(&request)?;
        prepare_pdf_fallback(&mut request, capabilities).await?;
        let key = api_key.ok_or_else(|| RouterError::unavailable("provider API key is missing"))?;
        let body = anthropic_request(model, &request)?;
        let mut builder = self
            .transport
            .client()
            .post(&self.endpoint)
            .header("x-api-key", key.expose_secret())
            .header("anthropic-version", "2023-06-01");
        if body.pointer("/thinking/block_binding").is_some() {
            builder = builder.header("anthropic-beta", THINKING_BINDING_BETA);
        }
        let retry = forces_tool(&body).then(|| builder.try_clone()).flatten();
        let mut response = builder.json(&body).send().await.map_err(request_error)?;
        // A model these rules do not know to refuse a forced tool call may
        // still answer 400 to it, as thinking does with any model. The refusal
        // costs nothing; ask once more and let the model choose the tool.
        if response.status() == http::StatusCode::BAD_REQUEST
            && let Some(builder) = retry
        {
            let mut unforced = body.clone();
            unforced["tool_choice"]["type"] = json!("auto");
            if let Some(choice) = unforced["tool_choice"].as_object_mut() {
                choice.remove("name");
            }
            response = builder
                .json(&unforced)
                .send()
                .await
                .map_err(request_error)?;
        }
        let response = self.transport.checked(response).await?;
        let upstream_request_id = upstream_request_id(response.headers());
        let mut source = self.transport.sse(response, cancel);
        let events = Box::pin(try_stream! {
            let mut thinking = std::collections::HashMap::<u32, String>::new();
            let mut prompt = PromptUsage::default();
            let mut output_tokens = 0;
            while let Some(event) = source.next().await {
                let event = event?;
                match event.get("type").and_then(Value::as_str).unwrap_or_default() {
                    "message_start" => {
                        if let Some(usage) = event.pointer("/message/usage") {
                            prompt.update(usage);
                            output_tokens = usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                            // Billed from here on, whatever happens to the stream.
                            yield ProviderEvent::PromptUsage(prompt.usage(output_tokens));
                        }
                    }
                    "content_block_start" => {
                        let block_index = event
                            .get("index")
                            .and_then(Value::as_u64)
                            .and_then(|value| u32::try_from(value).ok())
                            .unwrap_or(0);
                        let block = event.get("content_block").unwrap_or(&Value::Null);
                        match block.get("type").and_then(Value::as_str).unwrap_or_default() {
                            "text" => if let Some(text) = block.get("text").and_then(Value::as_str).filter(|text| !text.is_empty()) {
                                yield ProviderEvent::TextDelta { index: 0, text: text.into() };
                            },
                            "thinking" => if let Some(text) = block.get("thinking").and_then(Value::as_str).filter(|text| !text.is_empty()) {
                                thinking.entry(block_index).or_default().push_str(text);
                                yield ProviderEvent::ReasoningDelta { index: 0, text: text.into() };
                            },
                            "redacted_thinking" => if let Some(data) = block.get("data") {
                                yield ProviderEvent::ReasoningDetail { index: 0, detail: json!({
                                    "type": "reasoning.encrypted", "format": "anthropic-claude-v1",
                                    "index": block_index, "data": data
                                }) };
                            },
                            "tool_use" => yield ProviderEvent::ToolCallDelta {
                                choice_index: 0,
                                tool_index: block_index,
                                id: block.get("id").and_then(Value::as_str).map(str::to_owned),
                                kind: "function".into(),
                                name: block.get("name").and_then(Value::as_str).map(str::to_owned),
                                arguments: Some(String::new()),
                            },
                            _ => {}
                        }
                    }
                    "content_block_delta" => {
                        let block_index = event
                            .get("index")
                            .and_then(Value::as_u64)
                            .and_then(|value| u32::try_from(value).ok())
                            .unwrap_or(0);
                        let delta = event.get("delta").unwrap_or(&Value::Null);
                        match delta.get("type").and_then(Value::as_str).unwrap_or_default() {
                            "text_delta" => if let Some(text) = delta.get("text").and_then(Value::as_str) {
                                yield ProviderEvent::TextDelta { index: 0, text: text.into() };
                            },
                            "thinking_delta" => if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                                thinking.entry(block_index).or_default().push_str(text);
                                yield ProviderEvent::ReasoningDelta { index: 0, text: text.into() };
                            },
                            "signature_delta" => if let Some(signature) = delta.get("signature").and_then(Value::as_str) {
                                yield ProviderEvent::ReasoningDetail { index: 0, detail: json!({
                                    "type": "reasoning.text", "format": "anthropic-claude-v1",
                                    "index": block_index, "text": thinking.get(&block_index).cloned().unwrap_or_default(),
                                    "signature": signature
                                }) };
                            },
                            "input_json_delta" => yield ProviderEvent::ToolCallDelta {
                                choice_index: 0,
                                tool_index: block_index,
                                id: None,
                                kind: "function".into(),
                                name: None,
                                arguments: delta.get("partial_json").and_then(Value::as_str).map(str::to_owned),
                            },
                            _ => {}
                        }
                    }
                    "message_delta" => {
                        output_tokens = event.pointer("/usage/output_tokens").and_then(Value::as_u64).unwrap_or(output_tokens);
                        // Newer API versions repeat the (cumulative) prompt counters here.
                        if let Some(usage) = event.get("usage") {
                            prompt.update(usage);
                        }
                        yield ProviderEvent::Usage(prompt.usage(output_tokens));
                        if let Some(reason) = event.pointer("/delta/stop_reason").and_then(Value::as_str) {
                            yield ProviderEvent::Finish { index: 0, reason: normalize_finish(reason).into() };
                        }
                    }
                    "error" => Err(RouterError::upstream(
                        http::StatusCode::BAD_GATEWAY,
                        "Anthropic stream returned an error event",
                    ))?,
                    _ => {}
                }
            }
        });
        Ok(ProviderStream {
            events,
            upstream_request_id,
        })
    }
}

/// Anthropic's prompt counters. `input_tokens` excludes both cache reads and cache writes, which are billed at their
/// own rates (reads at 0.1×, five-minute writes at 1.25× and one-hour writes at 2× the input price), so all three are
/// kept and reported the way the Bedrock adapter does: `prompt_tokens` counts every prompt token, and
/// `prompt_tokens_details` says how many were read from (`cached_tokens`) and written to
/// (`cache_creation_input_tokens`, of which `cache_creation_1h_input_tokens` with a one-hour TTL) the cache.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PromptUsage {
    input: u64,
    cache_read: u64,
    cache_write: u64,
    cache_write_1h: u64,
}

impl PromptUsage {
    fn update(&mut self, usage: &Value) {
        let count = |pointer: &str| usage.pointer(pointer).and_then(Value::as_u64);
        if let Some(input) = count("/input_tokens") {
            self.input = input;
        }
        if let Some(read) = count("/cache_read_input_tokens") {
            self.cache_read = read;
        }
        if let Some(written) = count("/cache_creation_input_tokens") {
            self.cache_write = written;
        }
        if let Some(one_hour) = count("/cache_creation/ephemeral_1h_input_tokens") {
            self.cache_write_1h = one_hour.min(self.cache_write);
        }
    }

    fn usage(self, output_tokens: u64) -> Usage {
        let prompt_tokens = self
            .input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write);
        Usage {
            prompt_tokens,
            completion_tokens: output_tokens,
            total_tokens: prompt_tokens.saturating_add(output_tokens),
            prompt_tokens_details: (self.cache_read > 0 || self.cache_write > 0).then(|| {
                json!({
                    "cached_tokens": self.cache_read,
                    "cache_creation_input_tokens": self.cache_write,
                    "cache_creation_1h_input_tokens": self.cache_write_1h,
                })
            }),
            completion_tokens_details: None,
        }
    }
}

fn anthropic_request(model: &str, request: &ChatCompletionRequest) -> Result<Value, RouterError> {
    reject_unsupported_parameters(request)?;
    let mut system = Vec::new();
    let mut messages = Vec::new();
    for message in &request.messages {
        if matches!(message.role.as_str(), "system" | "developer") {
            let mut blocks = system_content_blocks(message)?;
            apply_message_cache_control(&mut blocks, message)?;
            system.extend(blocks);
            continue;
        }
        messages.push(anthropic_message(message)?);
    }
    let tools = request
        .tools
        .iter()
        .map(anthropic_tool)
        .collect::<Result<Vec<_>, _>>()?;
    let max_tokens = request
        .max_completion_tokens
        .or(request.max_tokens)
        .unwrap_or_else(|| default_anthropic_max_tokens(model, request));
    let mut body = json!({
        "model": model,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": true
    });
    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if accepts_sampling(model) {
        copy_number(&mut body, "temperature", request.temperature);
        copy_number(&mut body, "top_p", request.top_p);
    }
    if let Some(stop) = &request.stop {
        body["stop_sequences"] = match stop {
            Value::String(value) => json!([value]),
            value => value.clone(),
        };
    }
    if let Some(choice) = &request.tool_choice {
        body["tool_choice"] = anthropic_tool_choice(choice)?;
        if rejects_forced_tool_choice(model)
            && matches!(body["tool_choice"]["type"].as_str(), Some("tool" | "any"))
        {
            return Err(RouterError::invalid(
                "this Claude model supports only auto or none tool_choice",
                "tool_choice",
            ));
        }
    }
    apply_anthropic_reasoning(model, request, max_tokens, &mut body)?;
    if let Some(format) = &request.response_format {
        let output = body
            .as_object_mut()
            .expect("request body is an object")
            .entry("output_config")
            .or_insert_with(|| json!({}));
        match format.get("type").and_then(Value::as_str) {
            Some("json_schema") => {
                let schema = format.pointer("/json_schema/schema").ok_or_else(|| {
                    RouterError::invalid(
                        "json_schema response format requires schema",
                        "response_format",
                    )
                })?;
                output["format"] = json!({"type": "json_schema", "schema": schema});
            }
            Some("json_object") => {
                output["format"] = json!({"type": "json_schema", "schema": {"type": "object"}});
            }
            Some("text") | None => {}
            Some(other) => {
                return Err(RouterError::invalid(
                    format!("unsupported response format {other}"),
                    "response_format",
                ));
            }
        }
    }
    Ok(body)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum AnthropicThinkingMode {
    Manual,
    ManualWithEffort,
    Adaptive,
}

fn thinking_mode(model: &str) -> AnthropicThinkingMode {
    let model = model.to_ascii_lowercase();
    let model = model
        .rsplit_once('-')
        .filter(|(_, suffix)| suffix.len() == 8 && suffix.bytes().all(|b| b.is_ascii_digit()))
        .map_or(model.as_str(), |(family, _)| family);
    if model.contains("claude-opus-4-5") {
        return AnthropicThinkingMode::ManualWithEffort;
    }
    if model.contains("claude-haiku-4-5")
        || model.contains("claude-sonnet-4-5")
        || model.contains("claude-3")
        || model.contains("claude-sonnet-4-0")
        || model == "claude-sonnet-4"
        || model.contains("claude-opus-4-0")
        || model == "claude-opus-4"
        || model.contains("claude-opus-4-1")
        || model.contains("claude-4-0")
        || model.contains("claude-4-1")
    {
        return AnthropicThinkingMode::Manual;
    }
    AnthropicThinkingMode::Adaptive
}

pub(crate) fn uses_manual_thinking(model: &str) -> bool {
    thinking_mode(model) != AnthropicThinkingMode::Adaptive
}

/// Claude's generation as `[major, minor]` from ids such as `claude-opus-4-7`,
/// `claude-haiku-5-5` or `claude-3-5-sonnet-20241022`; snapshot dates are ignored.
fn claude_generation(model: &str) -> Vec<u64> {
    model
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty() && part.len() <= 2)
        .filter_map(|part| part.parse().ok())
        .take(2)
        .collect()
}

/// Claude 4.7 and later answer 400 ("deprecated for this model") to
/// `temperature` and `top_p`, so OpenAI-style defaults are left out for them.
fn accepts_sampling(model: &str) -> bool {
    !model.to_ascii_lowercase().contains("claude")
        || !matches!(
            claude_generation(model).as_slice(),
            [5..=99, ..] | [4, 7..=99, ..]
        )
}

const THINKING_BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";

/// A Claude id's family and `(major, minor)` generation: `("opus", (5, 5))`
/// for `claude-opus-5-5`, `("sonnet", (3, 5))` for `claude-3-5-sonnet-20241022`.
fn claude_model(model: &str) -> Option<(String, (u64, u64))> {
    let lower = model.to_ascii_lowercase();
    let mut parts = lower.split(['-', '.']);
    parts.find(|part| part.ends_with("claude"))?;
    let family = parts
        .find(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphabetic()))
        .unwrap_or_default()
        .to_owned();
    let generation = claude_generation(model);
    let major = *generation.first()?;
    Some((family, (major, generation.get(1).copied().unwrap_or(0))))
}

/// Models whose thinking blocks stay valid only while everything before them is
/// unchanged (Anthropic's preserved-thinking page, 2026-10-10): Fable from 5.1
/// and every family from 5.5. A newer generation is taken to do the same.
fn prefix_bound_thinking(model: &str) -> bool {
    claude_model(model).is_some_and(|(family, version)| {
        version >= (5, 5) || (family == "fable" && version >= (5, 1))
    })
}

/// Models that answer 400 to `tool_choice` `any` or `tool` (checked 2026-10-10):
/// Fable and Mythos from 5.1, Opus and Sonnet from 5.5; Sonnet 5 and every
/// Haiku so far accept them. A new family follows the newest flagships.
fn rejects_forced_tool_choice(model: &str) -> bool {
    claude_model(model).is_some_and(|(family, version)| match family.as_str() {
        "haiku" => false,
        "fable" | "mythos" => version >= (5, 1),
        _ => version >= (5, 5),
    })
}

/// Whether the body forces a tool call, which a model may refuse.
fn forces_tool(body: &Value) -> bool {
    matches!(
        body.pointer("/tool_choice/type").and_then(Value::as_str),
        Some("tool" | "any")
    )
}

fn reasoning_effort(request: &ChatCompletionRequest) -> Option<&str> {
    request
        .reasoning
        .as_ref()
        .and_then(|value| value.effort.as_deref())
        .or(request.reasoning_effort.as_deref())
}

fn manual_effort_budget(effort: &str) -> Result<u64, RouterError> {
    match effort {
        "minimal" | "low" => Ok(1_024),
        "medium" => Ok(4_096),
        "high" => Ok(8_192),
        "xhigh" => Ok(16_384),
        "max" => Ok(32_768),
        _ => Err(RouterError::invalid(
            format!("unsupported Anthropic reasoning effort {effort}"),
            "reasoning_effort",
        )),
    }
}

fn default_anthropic_max_tokens(model: &str, request: &ChatCompletionRequest) -> u64 {
    let reasoning = request.reasoning.as_ref();
    if reasoning.and_then(|value| value.enabled) == Some(false)
        || reasoning_effort(request) == Some("none")
    {
        return 1_024;
    }
    if let Some(budget) = reasoning.and_then(|value| value.max_tokens) {
        return budget.saturating_add(1_024);
    }
    if request.reasoning.is_some() || request.reasoning_effort.is_some() {
        return if thinking_mode(model) == AnthropicThinkingMode::Adaptive {
            8_192
        } else {
            reasoning_effort(request)
                .map(manual_effort_budget)
                .transpose()
                .ok()
                .flatten()
                .unwrap_or(4_096)
                .saturating_add(1_024)
        };
    }
    1_024
}

fn apply_anthropic_reasoning(
    model: &str,
    request: &ChatCompletionRequest,
    max_tokens: u64,
    body: &mut Value,
) -> Result<(), RouterError> {
    if request.reasoning.is_none() && request.reasoning_effort.is_none() {
        return Ok(());
    }
    let reasoning = request.reasoning.as_ref();
    let effort = reasoning_effort(request);
    if reasoning.and_then(|value| value.enabled) == Some(false) || effort == Some("none") {
        if reasoning.and_then(|value| value.enabled) == Some(false)
            && effort.is_some_and(|value| value != "none")
        {
            return Err(RouterError::invalid(
                "reasoning.enabled=false conflicts with reasoning effort",
                "reasoning",
            ));
        }
        body["thinking"] = json!({"type": "disabled"});
        return Ok(());
    }
    if accepts_sampling(model)
        && request
            .temperature
            .is_some_and(|temperature| (temperature - 1.0).abs() > f64::EPSILON)
    {
        return Err(RouterError::invalid(
            "Anthropic thinking requires temperature 1 or an omitted temperature",
            "temperature",
        ));
    }

    let mode = thinking_mode(model);
    if let Some(budget) = reasoning.and_then(|value| value.max_tokens) {
        validate_manual_budget(budget, max_tokens)?;
        body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
        if let Some(effort) = effort {
            if mode != AnthropicThinkingMode::ManualWithEffort {
                return Err(RouterError::invalid(
                    "this Anthropic model cannot combine reasoning effort with a manual token budget",
                    "reasoning.effort",
                ));
            }
            manual_effort_budget(effort)?;
            body["output_config"] = json!({"effort": effort});
        }
        return Ok(());
    }

    if mode == AnthropicThinkingMode::Adaptive {
        // Without `display`, adaptive thinking comes back as empty blocks.
        body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
        if prefix_bound_thinking(model) {
            // A changed prompt or tool list retires the invalidated thinking
            // instead of failing the conversation with a 400.
            body["thinking"]["block_binding"] = json!({"prefix_mismatch_behavior": "drop_block"});
        }
        if let Some(effort) = effort {
            manual_effort_budget(effort)?;
            body["output_config"] = json!({"effort": effort});
        }
        return Ok(());
    }

    let budget = effort.map_or(Ok(4_096), manual_effort_budget)?;
    validate_manual_budget(budget, max_tokens)?;
    body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
    if mode == AnthropicThinkingMode::ManualWithEffort
        && let Some(effort) = effort
    {
        body["output_config"] = json!({"effort": effort});
    }
    Ok(())
}

fn validate_manual_budget(budget: u64, max_tokens: u64) -> Result<(), RouterError> {
    if budget < 1_024 {
        return Err(RouterError::invalid(
            "Anthropic reasoning budget must be at least 1024 tokens",
            "reasoning.max_tokens",
        ));
    }
    if budget >= max_tokens {
        return Err(RouterError::invalid(
            "Anthropic reasoning budget must be less than the output token limit",
            "reasoning.max_tokens",
        ));
    }
    Ok(())
}

fn anthropic_message(message: &Message) -> Result<Value, RouterError> {
    if message.role == "tool" {
        let id = message.tool_call_id.as_deref().ok_or_else(|| {
            RouterError::invalid(
                "tool message requires tool_call_id",
                "messages.tool_call_id",
            )
        })?;
        let mut content = vec![json!({
            "type": "tool_result", "tool_use_id": id,
            "content": message_texts(message)?.join("\n")
        })];
        apply_message_cache_control(&mut content, message)?;
        return Ok(json!({"role": "user", "content": content}));
    }
    if message.role == "function" {
        return Err(RouterError::invalid(
            "legacy function messages cannot be translated losslessly to Anthropic",
            "messages.role",
        ));
    }
    let role = if message.role == "assistant" {
        "assistant"
    } else {
        "user"
    };
    let regular_content = content_blocks(message)?;
    let mut content = if role == "assistant" {
        Vec::new()
    } else {
        regular_content.clone()
    };
    if role == "assistant" {
        // Only Claude's own thinking goes back to Claude: another model's
        // reasoning in the same chat (DeepSeek's through OpenRouter, OpenAI's
        // encrypted items) has no Claude signature, and Anthropic refuses it.
        for detail in message
            .reasoning_details
            .iter()
            .filter(|detail| claude_reasoning(detail))
        {
            match detail.get("type").and_then(Value::as_str) {
                Some("reasoning.encrypted") => content.push(json!({
                    "type": "redacted_thinking", "data": detail.get("data").cloned().unwrap_or(Value::Null)
                })),
                Some("reasoning.text") => content.push(json!({
                    "type": "thinking",
                    "thinking": detail.get("text").and_then(Value::as_str).unwrap_or_default(),
                    "signature": detail.get("signature").and_then(Value::as_str).unwrap_or_default()
                })),
                _ => {}
            }
        }
        content.extend(regular_content);
        for call in &message.tool_calls {
            let function = call.function.as_ref().ok_or_else(|| {
                RouterError::invalid(
                    "function tool call is missing function",
                    "messages.tool_calls",
                )
            })?;
            let input: Value = serde_json::from_str(&function.arguments).map_err(|error| {
                RouterError::invalid(
                    format!("tool arguments are not valid JSON: {error}"),
                    "messages.tool_calls.function.arguments",
                )
            })?;
            content.push(
                json!({"type": "tool_use", "id": call.id, "name": function.name, "input": input}),
            );
        }
    }
    apply_message_cache_control(&mut content, message)?;
    Ok(json!({"role": role, "content": content}))
}

/// Whether a reasoning detail is Claude's own, which Claude can take back: an
/// Anthropic-format detail (`anthropic-claude-v1`), or one without a format
/// that carries a signature (older clients). A thinking block needs its
/// signature; a redacted one its data.
fn claude_reasoning(detail: &Value) -> bool {
    let format = detail.get("format").and_then(Value::as_str);
    let claude = format.is_none_or(|format| {
        let format = format.to_ascii_lowercase();
        format.contains("anthropic") || format.contains("claude")
    });
    let sealed = match detail.get("type").and_then(Value::as_str) {
        Some("reasoning.text") => detail
            .get("signature")
            .and_then(Value::as_str)
            .is_some_and(|signature| !signature.is_empty()),
        Some("reasoning.encrypted") => detail.get("data").is_some_and(|data| !data.is_null()),
        _ => false,
    };
    claude && sealed
}

/// Claude refuses a text block that is empty or only whitespace, such as the
/// one OpenAI-style clients send beside a photo that has no text, so blank
/// text is left out; it says nothing.
fn content_blocks(message: &Message) -> Result<Vec<Value>, RouterError> {
    let mut blocks = Vec::new();
    match &message.content {
        Some(MessageContent::Text(text)) if !text.trim().is_empty() => {
            blocks.push(json!({"type": "text", "text": text}));
        }
        Some(MessageContent::Text(_)) | None => {}
        Some(MessageContent::Parts(parts)) => {
            for part in parts {
                let mut block = match part.kind.as_str() {
                    "text" => match part.text.as_deref() {
                        Some(text) if !text.trim().is_empty() => {
                            json!({"type": "text", "text": text})
                        }
                        _ => continue,
                    },
                    "image_url" => {
                        let url = &part
                            .image_url
                            .as_ref()
                            .ok_or_else(|| {
                                RouterError::invalid(
                                    "image_url part is missing image_url",
                                    "messages.content.image_url",
                                )
                            })?
                            .url;
                        media_source("image", url)?
                    }
                    "file" => {
                        let data = part
                            .file
                            .as_ref()
                            .and_then(|file| file.file_data.as_ref())
                            .ok_or_else(|| {
                                RouterError::invalid(
                                    "file part is missing file_data",
                                    "messages.content.file.file_data",
                                )
                            })?;
                        media_source("document", data)?
                    }
                    other => {
                        return Err(RouterError::invalid(
                            format!("Anthropic cannot accept {other}"),
                            "messages.content.type",
                        ));
                    }
                };
                if let Some(cache_control) = cache_control(&part.extra, "messages.content")? {
                    block["cache_control"] = cache_control.clone();
                }
                blocks.push(block);
            }
        }
    }
    Ok(blocks)
}

fn system_content_blocks(message: &Message) -> Result<Vec<Value>, RouterError> {
    match &message.content {
        Some(MessageContent::Text(text)) => Ok(vec![json!({"type": "text", "text": text})]),
        Some(MessageContent::Parts(parts)) => parts
            .iter()
            .map(|part| {
                if part.kind != "text" {
                    return Err(RouterError::invalid(
                        "system content must be text for Anthropic",
                        "messages.content",
                    ));
                }
                let mut block =
                    json!({"type": "text", "text": part.text.as_deref().unwrap_or_default()});
                if let Some(cache_control) = cache_control(&part.extra, "messages.content")? {
                    block["cache_control"] = cache_control.clone();
                }
                Ok(block)
            })
            .collect(),
        None => Ok(Vec::new()),
    }
}

fn apply_message_cache_control(blocks: &mut [Value], message: &Message) -> Result<(), RouterError> {
    let Some(cache_control) = cache_control(&message.extra, "messages.cache_control")? else {
        return Ok(());
    };
    let block = blocks.last_mut().ok_or_else(|| {
        RouterError::invalid(
            "cache_control requires a non-empty message content block",
            "messages.cache_control",
        )
    })?;
    block["cache_control"] = cache_control.clone();
    Ok(())
}

fn media_source(kind: &str, value: &str) -> Result<Value, RouterError> {
    if value.starts_with("https://") {
        return Ok(json!({"type": kind, "source": {"type": "url", "url": value}}));
    }
    let (media_type, data) = parse_data_url(value)?;
    Ok(json!({"type": kind, "source": {"type": "base64", "media_type": media_type, "data": data}}))
}

fn parse_data_url(value: &str) -> Result<(&str, &str), RouterError> {
    let payload = value.strip_prefix("data:").ok_or_else(|| {
        RouterError::invalid("attachment must be HTTPS or a data URL", "messages.content")
    })?;
    let (metadata, data) = payload
        .split_once(',')
        .ok_or_else(|| RouterError::invalid("malformed data URL", "messages.content"))?;
    // `data:image/jpeg;name=photo.jpg;base64,` names the file too; the media
    // type is what comes before the first parameter.
    let mut metadata = metadata.split(';');
    let media_type = metadata.next().unwrap_or_default();
    if !metadata.any(|parameter| parameter == "base64") {
        return Err(RouterError::invalid(
            "data URL must use base64 encoding",
            "messages.content",
        ));
    }
    Ok((media_type, data))
}

fn message_texts(message: &Message) -> Result<Vec<String>, RouterError> {
    match &message.content {
        Some(MessageContent::Text(text)) => Ok(vec![text.clone()]),
        Some(MessageContent::Parts(parts)) => parts
            .iter()
            .map(|part| {
                if part.kind != "text" {
                    return Err(RouterError::invalid(
                        "system/tool content must be text for Anthropic",
                        "messages.content",
                    ));
                }
                Ok(part.text.clone().unwrap_or_default())
            })
            .collect(),
        None => Ok(Vec::new()),
    }
}

fn anthropic_tool(tool: &Tool) -> Result<Value, RouterError> {
    let function = tool
        .function
        .as_ref()
        .ok_or_else(|| RouterError::invalid("Anthropic supports function tools only", "tools"))?;
    Ok(
        json!({"name": function.name, "description": function.description, "input_schema": function.parameters}),
    )
}

fn anthropic_tool_choice(choice: &Value) -> Result<Value, RouterError> {
    if let Some(value) = choice.as_str() {
        return Ok(match value {
            "auto" => json!({"type": "auto"}),
            "required" => json!({"type": "any"}),
            "none" => json!({"type": "none"}),
            _ => {
                return Err(RouterError::invalid(
                    "unsupported tool_choice",
                    "tool_choice",
                ));
            }
        });
    }
    if let Some(name) = choice.pointer("/function/name").and_then(Value::as_str) {
        return Ok(json!({"type": "tool", "name": name}));
    }
    Err(RouterError::invalid(
        "unsupported tool_choice",
        "tool_choice",
    ))
}

fn copy_number(body: &mut Value, key: &str, value: Option<f64>) {
    if let Some(number) = value.and_then(serde_json::Number::from_f64) {
        body[key] = Value::Number(number);
    }
}

fn normalize_finish(reason: &str) -> &str {
    match reason {
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        "end_turn" | "stop_sequence" => "stop",
        "refusal" => "content_filter",
        other => other,
    }
}

fn validate_nested_extensions(request: &ChatCompletionRequest) -> Result<(), RouterError> {
    for message in &request.messages {
        cache_control(&message.extra, "messages")?;
        if let Some(MessageContent::Parts(parts)) = &message.content {
            for part in parts {
                cache_control(&part.extra, "messages.content")?;
            }
        }
    }
    if request.tools.iter().any(|tool| !tool.extra.is_empty())
        || request
            .reasoning
            .as_ref()
            .is_some_and(|value| !value.extra.is_empty())
        || request
            .stream_options
            .as_ref()
            .is_some_and(|value| !value.extra.is_empty())
    {
        return Err(RouterError::invalid(
            "unsupported Anthropic extension fields",
            "body",
        ));
    }
    Ok(())
}

fn cache_control<'a>(
    extra: &'a BTreeMap<String, Value>,
    parameter: &str,
) -> Result<Option<&'a Value>, RouterError> {
    if let Some(field) = extra.keys().find(|field| field.as_str() != "cache_control") {
        return Err(RouterError::invalid(
            format!("Anthropic adapter cannot preserve {field}"),
            format!("{parameter}.{field}"),
        ));
    }
    let Some(value) = extra.get("cache_control") else {
        return Ok(None);
    };
    let object = value.as_object().ok_or_else(|| {
        RouterError::invalid(
            "cache_control must be an object",
            format!("{parameter}.cache_control"),
        )
    })?;
    if let Some(field) = object
        .keys()
        .find(|field| !matches!(field.as_str(), "type" | "ttl"))
    {
        return Err(RouterError::invalid(
            format!("unsupported cache_control field {field}"),
            format!("{parameter}.cache_control.{field}"),
        ));
    }
    if object.get("type").and_then(Value::as_str) != Some("ephemeral") {
        return Err(RouterError::invalid(
            "cache_control.type must be ephemeral",
            format!("{parameter}.cache_control.type"),
        ));
    }
    if let Some(ttl) = object.get("ttl")
        && !matches!(ttl.as_str(), Some("5m" | "1h"))
    {
        return Err(RouterError::invalid(
            "cache_control.ttl must be 5m or 1h",
            format!("{parameter}.cache_control.ttl"),
        ));
    }
    Ok(Some(value))
}

fn reject_unsupported_parameters(request: &ChatCompletionRequest) -> Result<(), RouterError> {
    let unsupported = [
        (request.frequency_penalty.is_some(), "frequency_penalty"),
        (request.presence_penalty.is_some(), "presence_penalty"),
        (request.logit_bias.is_some(), "logit_bias"),
        (request.prediction.is_some(), "prediction"),
        (request.modalities.is_some(), "modalities"),
        (request.audio.is_some(), "audio"),
        (request.service_tier.is_some(), "service_tier"),
        (request.metadata.is_some(), "metadata"),
        (request.user.is_some(), "user"),
        (
            request.parallel_tool_calls == Some(false) && !request.tools.is_empty(),
            "parallel_tool_calls",
        ),
        (
            request
                .stream_options
                .as_ref()
                .and_then(|value| value.include_obfuscation)
                .is_some(),
            "stream_options.include_obfuscation",
        ),
    ];
    if let Some((_, parameter)) = unsupported.into_iter().find(|(present, _)| *present) {
        return Err(RouterError::invalid(
            format!("Anthropic adapter cannot preserve {parameter}"),
            parameter,
        ));
    }
    if request
        .tools
        .iter()
        .any(|tool| tool.function.as_ref().and_then(|function| function.strict) == Some(true))
    {
        return Err(RouterError::invalid(
            "Anthropic adapter cannot guarantee strict function schemas",
            "tools.function.strict",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use router_protocol::ChatCompletionRequest;
    use serde_json::json;

    use super::{PromptUsage, anthropic_request, validate_nested_extensions};

    #[test]
    fn only_claude_reasoning_is_replayed_to_claude() {
        // One chat: DeepSeek through OpenRouter, then GPT, then Claude, each with its reasoning.
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "claude-haiku-5-5",
            "messages": [
                {"role":"user","content":"read"},
                {"role":"assistant","content":"done","reasoning_details":[
                    {"type":"reasoning.text","text":"DeepSeek thinking","format":"unknown","index":0},
                    {"type":"reasoning.encrypted","data":"opaque","format":"openai-responses-v1","index":1},
                    {"type":"reasoning.summary","summary":"GPT summary","format":"openai-responses-v1","index":2},
                    {"type":"reasoning.text","text":"Claude thinking","signature":"sig","format":"anthropic-claude-v1","index":3},
                    {"type":"reasoning.encrypted","data":"redacted","format":"anthropic-claude-v1","index":4}
                ]},
                {"role":"user","content":"again"}
            ]
        }))
        .unwrap();
        let body = anthropic_request("claude-haiku-5-5", &request).unwrap();
        let content = &body["messages"][1]["content"];
        assert_eq!(
            content[0],
            json!({"type":"thinking","thinking":"Claude thinking","signature":"sig"})
        );
        assert_eq!(
            content[1],
            json!({"type":"redacted_thinking","data":"redacted"})
        );
        assert_eq!(content[2]["type"], "text");
        assert_eq!(content.as_array().unwrap().len(), 3, "{content}");
    }

    #[test]
    fn current_fable_and_mythos_reject_forced_tools_locally() {
        for model in [
            "claude-fable-5-1",
            "claude-mythos-5-1",
            "claude-opus-5-5",
            "claude-sonnet-5-5",
        ] {
            for choice in [
                json!("required"),
                json!({"type":"function","function":{"name":"weather"}}),
            ] {
                let request: ChatCompletionRequest = serde_json::from_value(json!({
                    "model": model, "messages": [{"role":"user","content":"weather"}],
                    "tool_choice": choice
                }))
                .unwrap();
                let error = anthropic_request(model, &request).unwrap_err();
                assert_eq!(error.body.param.as_deref(), Some("tool_choice"));
                assert!(anthropic_request("claude-haiku-4-5", &request).is_ok());
                assert!(anthropic_request("claude-haiku-5-5", &request).is_ok());
            }
            for choice in ["auto", "none"] {
                let request: ChatCompletionRequest = serde_json::from_value(json!({
                    "model": model, "messages": [{"role":"user","content":"weather"}],
                    "tool_choice": choice
                }))
                .unwrap();
                assert_eq!(
                    anthropic_request(model, &request).unwrap()["tool_choice"]["type"],
                    choice
                );
            }
        }
    }

    #[test]
    fn a_new_claude_model_inherits_the_newest_generation_rules() {
        use super::{prefix_bound_thinking, rejects_forced_tool_choice};
        // Every listed model keeps its answer.
        for (model, bound, refuses) in [
            ("claude-fable-5-1", true, true),
            ("claude-mythos-5-1", false, true),
            ("claude-opus-5-5", true, true),
            ("claude-sonnet-5-5", true, true),
            ("claude-haiku-5-5", true, false),
            ("claude-opus-5-5-20261001", true, true),
            ("claude-sonnet-5", false, false),
            ("claude-opus-5", false, false),
            ("claude-haiku-4-5-20251001", false, false),
            ("claude-3-5-sonnet-20241022", false, false),
            // Models no list named.
            ("claude-opus-6", true, true),
            ("claude-sonnet-5-7", true, true),
            ("claude-haiku-6-1", true, false),
            ("claude-mythos-5-5", true, true),
            ("claude-aria-6", true, true),
            ("gpt-9", false, false),
        ] {
            assert_eq!(prefix_bound_thinking(model), bound, "{model}");
            assert_eq!(rejects_forced_tool_choice(model), refuses, "{model}");
        }
    }

    #[test]
    fn leaves_out_sampling_that_new_claude_models_reject() {
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "anthropic/claude-haiku-5-5", "messages": [{"role":"user","content":"hi"}],
            "temperature": 0.2, "top_p": 0.9, "reasoning_effort": "low"
        }))
        .unwrap();
        for model in [
            "claude-haiku-5-5",
            "claude-sonnet-5-5",
            "claude-opus-5",
            "claude-opus-4-7",
        ] {
            let body = anthropic_request(model, &request).unwrap();
            assert!(body.get("temperature").is_none(), "{model}");
            assert!(body.get("top_p").is_none(), "{model}");
            assert_eq!(body["thinking"]["type"], "adaptive", "{model}");
        }
        let plain: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "anthropic/claude-sonnet-4-6", "messages": [{"role":"user","content":"hi"}],
            "temperature": 0.2, "top_p": 0.9
        }))
        .unwrap();
        for model in [
            "claude-sonnet-4-6",
            "claude-haiku-4-5-20251001",
            "claude-3-5-sonnet-20241022",
        ] {
            let body = anthropic_request(model, &plain).unwrap();
            assert_eq!(body["temperature"], 0.2, "{model}");
            assert_eq!(body["top_p"], 0.9, "{model}");
        }
        // Older families still refuse a non-default temperature with thinking.
        let thinking: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "anthropic/claude-sonnet-4-6", "messages": [{"role":"user","content":"hi"}],
            "temperature": 0.2, "reasoning_effort": "low", "max_completion_tokens": 8192
        }))
        .unwrap();
        assert!(anthropic_request("claude-sonnet-4-6", &thinking).is_err());
    }

    #[test]
    fn translates_tool_history_and_reasoning_signature() {
        let request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "anthropic/test",
            "messages": [{"role":"assistant","content":"", "reasoning_details":[{
                "type":"reasoning.text","format":"anthropic-claude-v1","text":"thinking","signature":"opaque"
            }], "tool_calls":[{"id":"call_1","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Tokyo\"}"}}]}]
        })).unwrap();
        let body = anthropic_request("test", &request).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["signature"], "opaque");
        assert_eq!(body["messages"][0]["content"][1]["type"], "tool_use");
    }

    #[test]
    fn a_photo_without_text_reaches_claude_without_a_blank_text_block() {
        let photo = json!({"type":"image_url","image_url":{
            "url":"data:image/jpeg;name=photo-1.jpg;base64,/9j/4AAQ"
        }});
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "anthropic/test",
            "messages": [
                {"role":"user", "content":[{"type":"text","text":""}, photo]},
                {"role":"assistant", "content":"A white app icon."},
                {"role":"user", "content":[{"type":"text","text":" \n"}, photo, {"type":"text","text":"and this?"}]}
            ]
        }))
        .unwrap();
        let body = anthropic_request("test", &request).unwrap();
        let image = json!({"type":"image","source":{
            "type":"base64","media_type":"image/jpeg","data":"/9j/4AAQ"
        }});
        assert_eq!(body["messages"][0]["content"], json!([image]));
        assert_eq!(
            body["messages"][2]["content"],
            json!([image, {"type":"text","text":"and this?"}])
        );
    }

    #[test]
    fn a_data_url_without_base64_is_refused() {
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "anthropic/test",
            "messages": [{"role":"user", "content":[{"type":"image_url","image_url":{
                "url":"data:image/png;name=base64.png,iVBORw0KGgo="
            }}]}]
        }))
        .unwrap();
        let error = anthropic_request("test", &request).unwrap_err();
        assert!(error.body.message.contains("base64"), "{error:?}");
    }

    #[test]
    fn translates_openai_compatible_cache_control() {
        let request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "anthropic/test",
            "messages": [
                {"role":"system", "content":"instructions", "cache_control":{"type":"ephemeral"}},
                {"role":"user", "content":[{
                    "type":"text", "text":"hello",
                    "cache_control":{"type":"ephemeral", "ttl":"1h"}
                }]}
            ]
        }))
        .unwrap();
        validate_nested_extensions(&request).unwrap();
        let body = anthropic_request("test", &request).unwrap();
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["ttl"],
            "1h"
        );
    }

    #[test]
    fn rejects_unknown_message_extension_by_name() {
        let request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "anthropic/test",
            "messages": [{"role":"user", "content":"hello", "mystery":true}]
        }))
        .unwrap();
        let error = validate_nested_extensions(&request).unwrap_err();
        assert_eq!(error.body.param.as_deref(), Some("messages.mystery"));
    }

    #[test]
    fn maps_openai_effort_to_manual_thinking_for_older_claude_families() {
        let request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "anthropic/claude-haiku-4-5-20251001",
            "messages": [{"role":"user", "content":"hello"}],
            "reasoning_effort": "low",
            "max_completion_tokens": 2048
        }))
        .unwrap();
        for model in [
            "claude-haiku-4-5-20251001",
            "claude-sonnet-4-20250514",
            "claude-opus-4-20250514",
            "claude-opus-4-1-20250805",
        ] {
            let body = anthropic_request(model, &request).unwrap();
            assert_eq!(body["thinking"]["type"], "enabled", "{model}");
            assert_eq!(body["thinking"]["budget_tokens"], 1_024, "{model}");
            assert!(body.get("output_config").is_none(), "{model}");
        }
    }

    #[test]
    fn keeps_effort_native_for_adaptive_thinking_models() {
        let request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "anthropic/claude-sonnet-4-6",
            "messages": [{"role":"user", "content":"hello"}],
            "reasoning_effort": "high",
            "max_completion_tokens": 8192
        }))
        .unwrap();
        let body = anthropic_request("claude-sonnet-4-6", &request).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "summarized");
        assert!(body["thinking"].get("block_binding").is_none());
        assert_eq!(body["output_config"]["effort"], "high");
        for model in ["claude-sonnet-5-5", "claude-haiku-5-5", "claude-opus-5-5"] {
            let body = anthropic_request(model, &request).unwrap();
            assert_eq!(
                body["thinking"]["block_binding"]["prefix_mismatch_behavior"], "drop_block",
                "{model}"
            );
        }
    }

    #[test]
    fn rejects_manual_thinking_budget_without_response_room() {
        let request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "anthropic/claude-haiku-4-5-20251001",
            "messages": [{"role":"user", "content":"hello"}],
            "reasoning_effort": "low",
            "max_completion_tokens": 1024
        }))
        .unwrap();
        let error = anthropic_request("claude-haiku-4-5-20251001", &request).unwrap_err();
        assert_eq!(error.body.param.as_deref(), Some("reasoning.max_tokens"));
    }

    #[test]
    fn prompt_usage_counts_cache_reads_and_writes() {
        let mut prompt = PromptUsage::default();
        prompt.update(&serde_json::json!({
            "input_tokens": 10,
            "cache_read_input_tokens": 1_000,
            "cache_creation_input_tokens": 300,
            "cache_creation": {"ephemeral_5m_input_tokens": 100, "ephemeral_1h_input_tokens": 200}
        }));
        let usage = prompt.usage(7);
        assert_eq!(
            usage.prompt_tokens, 1_310,
            "input_tokens excludes cache reads and writes"
        );
        assert_eq!(usage.total_tokens, 1_317);
        let details = usage.prompt_tokens_details.unwrap();
        assert_eq!(details["cached_tokens"], 1_000);
        assert_eq!(details["cache_creation_input_tokens"], 300);
        assert_eq!(details["cache_creation_1h_input_tokens"], 200);

        // A later message_delta repeating only some counters keeps the others.
        prompt.update(&serde_json::json!({"output_tokens": 9, "input_tokens": 10}));
        assert_eq!(prompt.usage(9).prompt_tokens, 1_310);

        // Without caching nothing changes for clients.
        let mut plain = PromptUsage::default();
        plain.update(&serde_json::json!({"input_tokens": 4}));
        let usage = plain.usage(2);
        assert_eq!((usage.prompt_tokens, usage.total_tokens), (4, 6));
        assert!(usage.prompt_tokens_details.is_none());
    }
}
