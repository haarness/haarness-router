use std::collections::BTreeMap;

use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use router_core::{
    Capabilities, EventStream, ProviderAdapter, ProviderConfig, ProviderEvent, ProviderStream,
    RouterError,
};
use router_protocol::{ChatCompletionRequest, Message, MessageContent, Usage};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    attachment::prepare_pdf_fallback,
    sse::{request_error, upstream_request_id},
    transport::ProviderTransport,
};

pub struct GeminiAdapter {
    transport: ProviderTransport,
    base_url: String,
    bearer_auth: bool,
}

impl GeminiAdapter {
    pub fn new(config: &ProviderConfig) -> Result<Self, RouterError> {
        Ok(Self {
            transport: ProviderTransport::new(config)?,
            base_url: config.effective_base_url()?,
            bearer_auth: config.auth_header.as_deref() == Some("authorization"),
        })
    }
}

#[async_trait]
impl ProviderAdapter for GeminiAdapter {
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
        let key = api_key.ok_or_else(|| RouterError::unavailable("provider API key is missing"))?;
        prepare_pdf_fallback(&mut request, capabilities).await?;
        // The router target, not a public alias, determines thinking syntax.
        request.model = model.to_owned();
        let body = gemini_request(&request)?;
        let url = format!(
            "{}/models/{model}:streamGenerateContent?alt=sse",
            self.base_url.trim_end_matches('/')
        );
        let request = self.transport.client().post(url);
        let request = if self.bearer_auth {
            request.bearer_auth(key.expose_secret())
        } else {
            request.header("x-goog-api-key", key.expose_secret())
        };
        let response = request.json(&body).send().await.map_err(request_error)?;
        let response = self.transport.checked(response).await?;
        let upstream_request_id = upstream_request_id(response.headers());
        let mut source = self.transport.sse(response, cancel);
        let events = Box::pin(try_stream! {
            // Gemini sends complete calls, but different calls may arrive in different
            // SSE events. Part positions reset in each event and are not tool indexes.
            let mut tool_indexes: BTreeMap<u32, u32> = BTreeMap::new();
            while let Some(event) = source.next().await {
                let event = event?;
                if let Some(error) = event.get("error") {
                    let status = error.get("status").and_then(Value::as_str).unwrap_or("UNKNOWN");
                    Err(RouterError::upstream(http::StatusCode::BAD_GATEWAY, format!("Gemini stream returned {status}")))?;
                }
                if let Some(usage) = event.get("usageMetadata") {
                    yield ProviderEvent::Usage(gemini_usage(usage));
                }
                let Some(candidates) = event.get("candidates").and_then(Value::as_array) else { continue; };
                for candidate in candidates {
                    let index = candidate
                        .get("index")
                        .and_then(Value::as_u64)
                        .and_then(|value| u32::try_from(value).ok())
                        .unwrap_or(0);
                    if let Some(parts) = candidate.pointer("/content/parts").and_then(Value::as_array) {
                        for part in parts {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                if part.get("thought").and_then(Value::as_bool) == Some(true) {
                                    yield ProviderEvent::ReasoningDelta { index, text: text.into() };
                                } else {
                                    yield ProviderEvent::TextDelta { index, text: text.into() };
                                }
                            }
                            if let Some(call) = part.get("functionCall") {
                                let next_index = tool_indexes.entry(index).or_default();
                                let tool_index = *next_index;
                                *next_index += 1;
                                let id = call.get("id").and_then(Value::as_str)
                                    .filter(|id| !id.is_empty())
                                    .map_or_else(|| format!("call_{}", Uuid::now_v7()), str::to_owned);
                                if let Some(signature) = part.get("thoughtSignature").and_then(Value::as_str) {
                                    yield ProviderEvent::ReasoningDetail { index, detail: json!({
                                        "type": "reasoning.encrypted", "format": "google-gemini-v1",
                                        "index": tool_index, "id": id, "data": signature
                                    }) };
                                }
                                yield ProviderEvent::ToolCallDelta {
                                    choice_index: index,
                                    tool_index,
                                    id: Some(id),
                                    kind: "function".into(),
                                    name: call.get("name").and_then(Value::as_str).map(str::to_owned),
                                    arguments: Some(call.get("args").cloned().unwrap_or_else(|| json!({})).to_string()),
                                };
                            } else if let Some(signature) = part.get("thoughtSignature").and_then(Value::as_str) {
                                // Keep signed non-call parts distinct from call signatures.
                                yield ProviderEvent::ReasoningDetail { index, detail: json!({
                                    "type": "reasoning.encrypted", "format": "google-gemini-v1",
                                    "id": format!("part_{}", Uuid::now_v7()), "data": signature,
                                    "part": part
                                }) };
                            }
                        }
                    }
                    if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                        let reason = if reason == "STOP" && tool_indexes.get(&index).copied().unwrap_or(0) > 0 {
                            "tool_calls"
                        } else { normalize_finish(reason) };
                        yield ProviderEvent::Finish { index, reason: reason.into() };
                    }
                }
            }
        });
        Ok(ProviderStream {
            events,
            upstream_request_id,
        })
    }
}

fn gemini_usage(usage: &Value) -> Usage {
    let input = usage["promptTokenCount"].as_u64().unwrap_or(0);
    let thoughts = usage["thoughtsTokenCount"].as_u64().unwrap_or(0);
    // Gemini reports thoughts separately from candidates. OpenAI-compatible
    // completion_tokens includes both, with reasoning_tokens a subset.
    let output = usage["candidatesTokenCount"]
        .as_u64()
        .unwrap_or(0)
        .saturating_add(thoughts);
    Usage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: usage["totalTokenCount"]
            .as_u64()
            .unwrap_or(input.saturating_add(output)),
        prompt_tokens_details: Some(
            json!({"cached_tokens": usage["cachedContentTokenCount"].as_u64().unwrap_or(0)}),
        ),
        completion_tokens_details: Some(json!({"reasoning_tokens": thoughts})),
    }
}

fn gemini_request(request: &ChatCompletionRequest) -> Result<Value, RouterError> {
    reject_unsupported_parameters(request)?;
    let call_names = request
        .messages
        .iter()
        .flat_map(|message| &message.tool_calls)
        .filter_map(|call| {
            call.function
                .as_ref()
                .map(|function| (call.id.as_str(), function.name.as_str()))
        })
        .collect::<std::collections::HashMap<_, _>>();
    let mut system_parts = Vec::new();
    let mut contents = Vec::new();
    let mut previous_was_tool = false;
    for message in &request.messages {
        if matches!(message.role.as_str(), "system" | "developer") {
            system_parts.extend(gemini_parts(message)?);
            continue;
        }
        if message.role == "function" {
            return Err(RouterError::invalid(
                "legacy function messages cannot be translated losslessly to Gemini",
                "messages.role",
            ));
        }
        let role = if message.role == "assistant" {
            "model"
        } else {
            "user"
        };
        let mut parts = gemini_parts(message)?;
        if message.role == "assistant" {
            restore_signed_parts(message, &mut parts);
            for (index, call) in message.tool_calls.iter().enumerate() {
                let function = call.function.as_ref().ok_or_else(|| {
                    RouterError::invalid(
                        "function tool call is missing function",
                        "messages.tool_calls",
                    )
                })?;
                let arguments: Value =
                    serde_json::from_str(&function.arguments).map_err(|error| {
                        RouterError::invalid(
                            format!("tool arguments are not valid JSON: {error}"),
                            "messages.tool_calls.function.arguments",
                        )
                    })?;
                let mut part = json!({"functionCall": {"id": call.id, "name": function.name, "args": arguments}});
                if let Some(signature) = reasoning_signature(message, index) {
                    part["thoughtSignature"] = Value::String(signature.into());
                }
                parts.push(part);
            }
        }
        if message.role == "tool" {
            let name = message
                .name
                .as_deref()
                .or_else(|| {
                    message
                        .tool_call_id
                        .as_deref()
                        .and_then(|id| call_names.get(id).copied())
                })
                .ok_or_else(|| {
                    RouterError::invalid(
                        "Gemini tool result cannot be matched to a prior tool call",
                        "messages.tool_call_id",
                    )
                })?;
            let output = message_text(message);
            parts = vec![json!({"functionResponse": {
                "id": message.tool_call_id, "name": name, "response": {"output": output}
            }})];
        }
        if message.role == "tool" && previous_was_tool {
            if let Some(previous) = contents
                .last_mut()
                .and_then(|value: &mut Value| value["parts"].as_array_mut())
            {
                previous.extend(parts);
            }
        } else {
            contents.push(json!({"role": role, "parts": parts}));
        }
        previous_was_tool = message.role == "tool";
    }
    let declarations = request.tools.iter().map(|tool| {
        let function = tool.function.as_ref().ok_or_else(|| RouterError::invalid("Gemini supports function tools only", "tools"))?;
        Ok(json!({"name": function.name, "description": function.description, "parametersJsonSchema": function.parameters}))
    }).collect::<Result<Vec<Value>, RouterError>>()?;
    let mut body = json!({"contents": contents});
    if !system_parts.is_empty() {
        body["systemInstruction"] = json!({"parts": system_parts});
    }
    if !declarations.is_empty() {
        body["tools"] = json!([{"functionDeclarations": declarations}]);
    }
    if let Some(choice) = &request.tool_choice {
        let config = gemini_tool_choice(choice)?;
        if config["mode"] == "ANY"
            && let Some(declarations) = body.pointer_mut("/tools/0/functionDeclarations")
        {
            // Forced calling compiles each schema into a decoding grammar, and
            // Gemini answers 400 "invalid argument" to an array bound over
            // complex items (e.g. 100 `anyOf` objects); clients validate calls.
            strip_max_items(declarations);
        }
        body["toolConfig"] = json!({"functionCallingConfig": config});
    }
    let mut generation = json!({});
    if let Some(value) = request.max_completion_tokens.or(request.max_tokens) {
        generation["maxOutputTokens"] = value.into();
    }
    copy_number(&mut generation, "temperature", request.temperature);
    copy_number(&mut generation, "topP", request.top_p);
    copy_number(
        &mut generation,
        "frequencyPenalty",
        request.frequency_penalty,
    );
    copy_number(&mut generation, "presencePenalty", request.presence_penalty);
    if let Some(stop) = &request.stop {
        generation["stopSequences"] = match stop {
            Value::String(value) => json!([value]),
            value => value.clone(),
        };
    }
    if let Some(format) = &request.response_format {
        if let Some(schema) = format
            .get("json_schema")
            .and_then(|value| value.get("schema"))
        {
            generation["responseMimeType"] = json!("application/json");
            generation["responseJsonSchema"] = schema.clone();
        } else if format.get("type").and_then(Value::as_str) == Some("json_object") {
            generation["responseMimeType"] = json!("application/json");
        }
    }
    if request.reasoning.is_some() || request.reasoning_effort.is_some() {
        let reasoning = request.reasoning.as_ref();
        let mut thinking =
            json!({"includeThoughts": reasoning.and_then(|value| value.exclude) != Some(true)});
        let effort = reasoning
            .and_then(|value| value.effort.as_deref())
            .or(request.reasoning_effort.as_deref());
        let disabled = reasoning.and_then(|value| value.enabled) == Some(false);
        if disabled && effort.is_some_and(|value| value != "none") {
            return Err(RouterError::invalid(
                "reasoning.enabled=false conflicts with reasoning effort",
                "reasoning",
            ));
        }
        let effort = if disabled { Some("none") } else { effort };
        let budget = reasoning.and_then(|value| value.max_tokens);
        if budget.is_some() && effort.is_some() {
            return Err(RouterError::invalid(
                "choose a reasoning effort or token budget, not both",
                "reasoning",
            ));
        }
        if let Some(tokens) = budget {
            thinking["thinkingBudget"] = tokens.into();
        } else if let Some(effort) = effort {
            let levels = crate::models::gemini_reasoning_efforts(&request.model);
            if !levels.is_empty() && !levels.contains(&effort) {
                return Err(RouterError::invalid(
                    format!(
                        "{} does not support reasoning effort {effort}",
                        request.model
                    ),
                    "reasoning_effort",
                ));
            }
            if request.model.starts_with("gemini-2.5") {
                let budget = match effort {
                    "none" => 0,
                    "low" => 2_048,
                    "medium" => 8_192,
                    "high" => 24_576,
                    _ => {
                        return Err(RouterError::invalid(
                            "unsupported Gemini reasoning effort",
                            "reasoning_effort",
                        ));
                    }
                };
                thinking["thinkingBudget"] = json!(budget);
            } else {
                thinking["thinkingLevel"] = Value::String(effort.to_owned());
            }
        }
        generation["thinkingConfig"] = thinking;
    }
    if generation
        .as_object()
        .is_some_and(|object| !object.is_empty())
    {
        body["generationConfig"] = generation;
    }
    Ok(body)
}

fn gemini_parts(message: &Message) -> Result<Vec<Value>, RouterError> {
    match &message.content {
        Some(MessageContent::Text(text)) => Ok(vec![json!({"text": text})]),
        Some(MessageContent::Parts(parts)) => parts.iter().map(|part| match part.kind.as_str() {
            "text" => Ok(json!({"text": part.text.as_deref().unwrap_or_default()})),
            "image_url" => media_part(&part.image_url.as_ref().ok_or_else(|| RouterError::invalid("image_url part is missing image_url", "messages.content.image_url"))?.url),
            "file" => media_part(part.file.as_ref().and_then(|file| file.file_data.as_deref()).ok_or_else(|| RouterError::invalid("file part is missing file_data", "messages.content.file.file_data"))?),
            "video_url" => media_part(&part.video_url.as_ref().ok_or_else(|| RouterError::invalid("video_url part is missing video_url", "messages.content.video_url"))?.url),
            "input_audio" => {
                let audio = part.input_audio.as_ref().ok_or_else(|| RouterError::invalid("input_audio part is missing input_audio", "messages.content.input_audio"))?;
                Ok(json!({"inlineData": {"mimeType": audio_mime(&audio.format)?, "data": audio.data}}))
            }
            other => Err(RouterError::invalid(format!("Gemini cannot accept {other}"), "messages.content.type")),
        }).collect(),
        None => Ok(Vec::new()),
    }
}

fn media_part(value: &str) -> Result<Value, RouterError> {
    if value.starts_with("https://") {
        return Ok(json!({"fileData": {"fileUri": value}}));
    }
    let payload = value.strip_prefix("data:").ok_or_else(|| {
        RouterError::invalid("attachment must be HTTPS or a data URL", "messages.content")
    })?;
    let (metadata, data) = payload
        .split_once(',')
        .ok_or_else(|| RouterError::invalid("malformed data URL", "messages.content"))?;
    let mime = metadata.strip_suffix(";base64").ok_or_else(|| {
        RouterError::invalid("data URL must use base64 encoding", "messages.content")
    })?;
    Ok(json!({"inlineData": {"mimeType": mime, "data": data}}))
}

fn audio_mime(format: &str) -> Result<&'static str, RouterError> {
    match format {
        "wav" => Ok("audio/wav"),
        "mp3" => Ok("audio/mpeg"),
        "flac" => Ok("audio/flac"),
        "aac" => Ok("audio/aac"),
        "ogg" => Ok("audio/ogg"),
        _ => Err(RouterError::invalid(
            "unsupported input_audio format",
            "messages.content.input_audio.format",
        )),
    }
}

// Match call signatures by the stable call ID, with ordinal support for older
// clients. A signed text/thought part must never be applied to a function call.
fn reasoning_signature(message: &Message, index: usize) -> Option<&str> {
    message.reasoning_details.iter().find_map(|detail| {
        if detail.get("part").is_some()
            || detail.get("format").and_then(Value::as_str) != Some("google-gemini-v1")
        {
            return None;
        }
        let matches = detail.get("id").and_then(Value::as_str).map_or_else(
            || detail.get("index").and_then(Value::as_u64) == Some(index as u64),
            |id| {
                message
                    .tool_calls
                    .get(index)
                    .is_some_and(|call| call.id == id)
            },
        );
        matches
            .then(|| {
                detail
                    .get("data")
                    .or_else(|| detail.get("signature"))
                    .and_then(Value::as_str)
            })
            .flatten()
    })
}

fn restore_signed_parts(message: &Message, parts: &mut Vec<Value>) {
    let mut thoughts = Vec::new();
    for detail in &message.reasoning_details {
        if detail.get("format").and_then(Value::as_str) != Some("google-gemini-v1") {
            continue;
        }
        let Some(part) = detail
            .get("part")
            .filter(|part| part.get("thoughtSignature").is_some())
        else {
            continue;
        };
        if part.get("thought") == Some(&Value::Bool(true)) {
            thoughts.push(part.clone());
        } else if let Some(text) = part.get("text").and_then(Value::as_str) {
            // A streamed final signed text part is often only the final suffix
            // (or empty). Keep its original bytes and split it out of the
            // aggregated assistant text instead of duplicating it.
            let location = parts.iter().position(|current| {
                current
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|value| value.ends_with(text))
            });
            if let Some(location) = location {
                let current = parts[location]["text"].as_str().unwrap_or_default();
                let prefix = &current[..current.len() - text.len()];
                if prefix.is_empty() {
                    parts[location] = part.clone();
                } else {
                    parts[location]["text"] = json!(prefix);
                    parts.insert(location + 1, part.clone());
                }
            } else {
                parts.push(part.clone());
            }
        }
    }
    parts.splice(0..0, thoughts);
}

fn message_text(message: &Message) -> String {
    match &message.content {
        Some(MessageContent::Text(text)) => text.clone(),
        Some(MessageContent::Parts(parts)) => parts
            .iter()
            .filter_map(|part| part.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    }
}

fn strip_max_items(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("maxItems");
            object.values_mut().for_each(strip_max_items);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_max_items),
        _ => {}
    }
}

fn gemini_tool_choice(choice: &Value) -> Result<Value, RouterError> {
    if let Some(value) = choice.as_str() {
        return Ok(match value {
            "none" => json!({"mode": "NONE"}),
            "auto" => json!({"mode": "AUTO"}),
            "required" => json!({"mode": "ANY"}),
            _ => {
                return Err(RouterError::invalid(
                    "unsupported tool_choice",
                    "tool_choice",
                ));
            }
        });
    }
    if let Some(name) = choice.pointer("/function/name").and_then(Value::as_str) {
        return Ok(json!({"mode": "ANY", "allowedFunctionNames": [name]}));
    }
    Err(RouterError::invalid(
        "unsupported tool_choice",
        "tool_choice",
    ))
}

fn reject_unsupported_parameters(request: &ChatCompletionRequest) -> Result<(), RouterError> {
    for message in &request.messages {
        if !message.extra.is_empty()
            || matches!(&message.content, Some(MessageContent::Parts(parts)) if parts.iter().any(|part| !part.extra.is_empty()))
        {
            return Err(RouterError::invalid(
                "unsupported Gemini message fields",
                "messages",
            ));
        }
    }
    let unsupported = [
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
            format!("Gemini adapter cannot preserve {parameter}"),
            parameter,
        ));
    }
    if request.tools.iter().any(|tool| {
        !tool.extra.is_empty()
            || tool.function.as_ref().and_then(|function| function.strict) == Some(true)
    }) {
        return Err(RouterError::invalid(
            "Gemini adapter cannot preserve strict or extended tool fields",
            "tools",
        ));
    }
    if request
        .reasoning
        .as_ref()
        .is_some_and(|value| !value.extra.is_empty())
        || request
            .stream_options
            .as_ref()
            .is_some_and(|value| !value.extra.is_empty())
    {
        return Err(RouterError::invalid(
            "unsupported Gemini extension fields",
            "body",
        ));
    }
    Ok(())
}

fn copy_number(body: &mut Value, key: &str, value: Option<f64>) {
    if let Some(number) = value.and_then(serde_json::Number::from_f64) {
        body[key] = Value::Number(number);
    }
}

fn normalize_finish(reason: &str) -> &str {
    match reason {
        "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "SAFETY" | "BLOCKLIST" | "PROHIBITED_CONTENT" => "content_filter",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use router_protocol::ChatCompletionRequest;
    use serde_json::json;

    use super::gemini_request;

    #[test]
    fn usage_includes_reasoning_output_and_cached_input() {
        let usage = super::gemini_usage(
            &json!({"promptTokenCount": 100, "candidatesTokenCount": 5, "thoughtsTokenCount": 40, "cachedContentTokenCount": 60}),
        );
        assert_eq!(
            (
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.total_tokens
            ),
            (100, 45, 145)
        );
        assert_eq!(usage.prompt_tokens_details.unwrap()["cached_tokens"], 60);
        assert_eq!(
            usage.completion_tokens_details.unwrap()["reasoning_tokens"],
            40
        );
        let plain = super::gemini_usage(
            &json!({"promptTokenCount": 3, "candidatesTokenCount": 2, "totalTokenCount": 5}),
        );
        assert_eq!(plain.completion_tokens, 2);
        assert_eq!(plain.total_tokens, 5);
    }

    #[test]
    fn every_advertised_gemini_effort_uses_its_generation_contract() {
        for model in [
            "gemini-4-argon",
            "gemini-3.8-flash",
            "gemini-3.5-flash-lite",
            "gemini-3-pro-preview",
            "gemini-2.5-pro",
            "gemini-2.5-flash",
        ] {
            for effort in crate::models::gemini_reasoning_efforts(model) {
                let request = serde_json::from_value(json!({"model":model,"messages":[{"role":"user","content":"hi"}],"reasoning_effort":effort})).unwrap();
                let body = gemini_request(&request).unwrap();
                let thinking = &body["generationConfig"]["thinkingConfig"];
                if model.starts_with("gemini-2.5") {
                    assert!(thinking["thinkingLevel"].is_null());
                    assert!(thinking["thinkingBudget"].is_number());
                } else {
                    assert_eq!(thinking["thinkingLevel"], effort);
                    assert!(thinking["thinkingBudget"].is_null());
                }
                assert_eq!(thinking["includeThoughts"], true);
            }
        }
        for (model, effort) in [
            ("gemini-4-argon", "minimal"),
            ("gemini-4-argon", "none"),
            ("gemini-3.8-flash", "minimal"),
            ("gemini-3-pro-preview", "medium"),
            ("gemini-2.5-pro", "none"),
        ] {
            let request = serde_json::from_value(
                json!({"model":model,"messages":[],"reasoning_effort":effort}),
            )
            .unwrap();
            assert!(gemini_request(&request).is_err());
        }
    }

    #[test]
    fn preserves_json_schemas_for_haarness_tools_and_structured_output() {
        let schema = serde_json::json!({
            "type": "object", "additionalProperties": false,
            "properties": {
                "agent": {"type": ["string", "null"]},
                "additionalProperties": {"type": "boolean"},
                "schedule": {"oneOf": [
                    {"type":"object", "properties":{"kind":{"const":"cron"}}},
                    {"type":"object", "properties":{"kind":{"const":"interval"}}}
                ]}
            }
        });
        let request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model":"gemini/test", "messages":[{"role":"user","content":"hi"}],
            "tools":[{"type":"function","function":{"name":"schedule","parameters":schema}}],
            "response_format":{"type":"json_schema","json_schema":{"name":"result","schema":schema}}
        }))
        .unwrap();
        let body = gemini_request(&request).unwrap();
        let declaration = &body["tools"][0]["functionDeclarations"][0];
        assert_eq!(declaration["parametersJsonSchema"], schema);
        assert!(declaration.get("parameters").is_none());
        assert_eq!(body["generationConfig"]["responseJsonSchema"], schema);
    }

    #[test]
    fn forced_tool_calls_leave_out_array_bounds_gemini_cannot_compile() {
        let schema = serde_json::json!({"type":"object","properties":{"fields":{
            "type":"array","maxItems":100,"minItems":1,"items":{"anyOf":[{"type":"object"}]}
        }}});
        for (choice, stripped) in [
            (serde_json::json!("required"), true),
            (
                serde_json::json!({"type":"function","function":{"name":"fill"}}),
                true,
            ),
            (serde_json::json!("auto"), false),
        ] {
            let request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
                "model":"gemini/test", "messages":[{"role":"user","content":"hi"}],
                "tools":[{"type":"function","function":{"name":"fill","parameters":schema}}],
                "tool_choice": choice
            }))
            .unwrap();
            let body = gemini_request(&request).unwrap();
            let fields = &body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["properties"]
                ["fields"];
            assert_eq!(fields.get("maxItems").is_none(), stripped, "{choice}");
            assert_eq!(fields["minItems"], 1);
        }
    }

    #[test]
    fn preserves_gemini_thought_signature_on_tool_history() {
        let request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model":"gemini/test",
            "messages":[{"role":"assistant","reasoning_details":[{
                "type":"reasoning.encrypted","format":"google-gemini-v1","index":0,"data":"opaque"
            }],"tool_calls":[{"id":"call","type":"function","function":{"name":"weather","arguments":"{}"}}]}]
        })).unwrap();
        let body = gemini_request(&request).unwrap();
        assert_eq!(
            body["contents"][0]["parts"][0]["thoughtSignature"],
            "opaque"
        );
    }

    #[test]
    fn signed_text_and_thought_parts_are_replayed_without_duplicate_text_or_call_signatures() {
        let request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model":"gemini/test", "messages":[{"role":"assistant","content":"Answer: 42",
                "reasoning_details":[
                    {"format":"google-gemini-v1","part":{"thought":true,"text":"first","thoughtSignature":"thought-1"}},
                    {"format":"google-gemini-v1","part":{"thought":true,"text":"second","thoughtSignature":"thought-2"}},
                    {"format":"google-gemini-v1","part":{"text":"42","thoughtSignature":"text-signature"}}
                ],
                "tool_calls":[{"id":"call-1","type":"function","function":{"name":"read","arguments":"{}"}}]
            }]
        })).unwrap();
        let body = gemini_request(&request).unwrap();
        let parts = body["contents"][0]["parts"].as_array().unwrap();
        assert_eq!(parts[0]["text"], "first");
        assert_eq!(parts[1]["text"], "second");
        assert_eq!(parts[2]["text"], "Answer: ");
        assert_eq!(parts[3]["text"], "42");
        assert_eq!(parts[3]["thoughtSignature"], "text-signature");
        assert!(parts[4].get("thoughtSignature").is_none());
    }
}
