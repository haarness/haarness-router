use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use router_core::{
    Capabilities, EventStream, ProviderAdapter, ProviderConfig, ProviderEvent, ProviderKind,
    ProviderStream, RouterError,
};
use router_protocol::{ChatCompletionRequest, Usage};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    attachment::prepare_pdf_fallback,
    sse::{request_error, upstream_request_id},
    transport::ProviderTransport,
};

pub struct OpenAiCompatibleAdapter {
    transport: ProviderTransport,
    endpoint: String,
    native_openai: bool,
    kind: ProviderKind,
    auth_header: reqwest::header::HeaderName,
    auth_scheme: Option<String>,
}

impl OpenAiCompatibleAdapter {
    pub fn new(config: &ProviderConfig) -> Result<Self, RouterError> {
        let custom_header = config.auth_header.as_deref();
        let auth_header = custom_header.map_or_else(
            || Ok(reqwest::header::AUTHORIZATION),
            |header| {
                reqwest::header::HeaderName::from_bytes(header.as_bytes()).map_err(|_| {
                    RouterError::invalid("invalid provider authentication header", "auth_header")
                })
            },
        )?;
        let auth_scheme = if custom_header.is_some() {
            config.auth_scheme.clone()
        } else {
            Some("Bearer".into())
        };
        Ok(Self {
            transport: ProviderTransport::new(config)?,
            endpoint: format!(
                "{}/chat/completions",
                config.effective_base_url()?.trim_end_matches('/')
            ),
            native_openai: config.kind.is_native_openai(),
            kind: config.kind,
            auth_header,
            auth_scheme,
        })
    }
}

#[async_trait]
impl ProviderAdapter for OpenAiCompatibleAdapter {
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
        let auth_value = self.auth_scheme.as_ref().map_or_else(
            || key.expose_secret().to_owned(),
            |scheme| format!("{scheme} {}", key.expose_secret()),
        );
        let auth_value = reqwest::header::HeaderValue::from_str(&auth_value).map_err(|_| {
            RouterError::invalid("provider API key is not a valid header value", "provider")
        })?;
        prepare_pdf_fallback(&mut request, capabilities).await?;
        if self.native_openai {
            normalize_native_openai_request(&mut request)?;
            if crate::openai_responses::required(model, &request) {
                let endpoint = self
                    .endpoint
                    .strip_suffix("/chat/completions")
                    .unwrap_or(&self.endpoint);
                return crate::openai_responses::stream(
                    &self.transport,
                    self.transport
                        .client()
                        .post(format!("{endpoint}/responses"))
                        .header(&self.auth_header, auth_value),
                    model,
                    &request,
                    cancel,
                )
                .await;
            }
        }
        prepare_for_kind(self.kind, model, &mut request);
        request.model = model.to_owned();
        request.stream = true;
        request.stream_options.get_or_insert_default().include_usage = true;
        let response = self
            .transport
            .client()
            .post(&self.endpoint)
            .header(&self.auth_header, auth_value)
            .json(&request)
            .send()
            .await
            .map_err(request_error)?;
        let response = self.transport.checked(response).await?;
        let upstream_request_id = upstream_request_id(response.headers());
        let mut source = self.transport.sse(response, cancel);
        let events = Box::pin(try_stream! {
            while let Some(value) = source.next().await {
                let value = value?;
                if value.get("error").is_some_and(|error| !error.is_null()) {
                    Err(crate::sse::stream_error(&value))?;
                }
                if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
                    yield ProviderEvent::Usage(parse_usage(usage));
                }
                let Some(choices) = value.get("choices").and_then(Value::as_array) else {
                    continue;
                };
                for choice in choices {
                    let index = choice
                        .get("index")
                        .and_then(Value::as_u64)
                        .and_then(|value| u32::try_from(value).ok())
                        .unwrap_or(0);
                    let delta = choice.get("delta").unwrap_or(&Value::Null);
                    match delta.get("content") {
                        Some(Value::String(text)) => {
                            yield ProviderEvent::TextDelta { index, text: text.clone() };
                        }
                        // Mistral's thinking models send content as typed chunks.
                        Some(Value::Array(chunks)) => {
                            for (thinking, text) in content_chunks(chunks) {
                                yield if thinking {
                                    ProviderEvent::ReasoningDelta { index, text }
                                } else {
                                    ProviderEvent::TextDelta { index, text }
                                };
                            }
                        }
                        _ => {}
                    }
                    let text = delta
                        .get("reasoning")
                        .or_else(|| delta.get("reasoning_content"))
                        .and_then(Value::as_str);
                    let details = delta.get("reasoning_details").and_then(Value::as_array);
                    match (text, details) {
                        // One chunk with both: the details carry the same text, so they travel together.
                        (Some(text), Some(details)) if !details.is_empty() => {
                            yield ProviderEvent::Reasoning { index, text: text.into(), details: details.clone() };
                        }
                        (text, details) => {
                            if let Some(text) = text {
                                yield ProviderEvent::ReasoningDelta { index, text: text.into() };
                            }
                            for detail in details.into_iter().flatten() {
                                yield ProviderEvent::ReasoningDetail { index, detail: detail.clone() };
                            }
                        }
                    }
                    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                        for call in calls {
                            let tool_index = call
                                .get("index")
                                .and_then(Value::as_u64)
                                .and_then(|value| u32::try_from(value).ok())
                                .unwrap_or(0);
                            let kind = call.get("type").and_then(Value::as_str).unwrap_or("function");
                            let payload = if kind == "custom" {
                                call.get("custom").unwrap_or(&Value::Null)
                            } else {
                                call.get("function").unwrap_or(&Value::Null)
                            };
                            yield ProviderEvent::ToolCallDelta {
                                choice_index: index,
                                tool_index,
                                id: call.get("id").and_then(Value::as_str).map(str::to_owned),
                                kind: kind.into(),
                                name: payload.get("name").and_then(Value::as_str).map(str::to_owned),
                                arguments: payload
                                    .get(if kind == "custom" { "input" } else { "arguments" })
                                    .and_then(Value::as_str)
                                    .map(str::to_owned),
                            };
                        }
                    }
                    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                        yield ProviderEvent::Finish { index, reason: normalize_finish(reason).into() };
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

/// Text and thinking from a content array: `{"type":"text","text":…}` and
/// `{"type":"thinking","thinking":[{"type":"text","text":…}]}` (or a plain
/// string in `thinking`), in order. `true` marks thinking.
fn content_chunks(chunks: &[Value]) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    for chunk in chunks {
        match chunk.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = chunk.get("text").and_then(Value::as_str) {
                    out.push((false, text.to_owned()));
                }
            }
            Some("thinking") => match chunk.get("thinking") {
                Some(Value::String(text)) => out.push((true, text.clone())),
                Some(Value::Array(parts)) => {
                    for part in parts {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            out.push((true, text.to_owned()));
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    out
}

/// The `reasoning_effort` levels a model takes, where its provider refuses
/// the others with a 400 and its catalog does not say. Empty: pass through.
pub(crate) fn compatible_reasoning_efforts(
    kind: ProviderKind,
    model: &str,
) -> &'static [&'static str] {
    match kind {
        // Large 4 always thinks; Small 4, Medium and their aliases think or not.
        ProviderKind::Mistral if model.starts_with("mistral-large-4") => &["low", "medium", "high"],
        ProviderKind::Mistral
            if [
                "mistral-small",
                "mistral-medium",
                "magistral-",
                "mistral-vibe-cli",
            ]
            .iter()
            .any(|family| model.starts_with(family)) =>
        {
            &["none", "high"]
        }
        ProviderKind::Groq if model.starts_with("openai/gpt-oss") => &["low", "medium", "high"],
        ProviderKind::Groq if model.starts_with("qwen/qwen3") => &["none", "low", "medium", "high"],
        // GLM-5.3 and its Flash models always think; GLM-5.2 takes every level.
        ProviderKind::Zai if model.starts_with("glm-5.3") => &["low", "high", "max"],
        ProviderKind::Zai if model.starts_with("glm-5.2") => {
            &["none", "minimal", "low", "medium", "high", "xhigh", "max"]
        }
        // Earlier GLM models ignore `reasoning_effort` and think or not.
        ProviderKind::Zai if model.starts_with("glm-") => &["none", "high"],
        _ => &[],
    }
}

/// What a provider's Chat Completions refuses or needs that others do not.
/// Mistral refuses any reasoning field on an assistant message (422), and
/// Groq takes `reasoning` but refuses `reasoning_content` and
/// `reasoning_details` (400), although clients replay what this router
/// answered. Z.ai continues a model's thinking from `reasoning_content`, so a
/// replayed `reasoning` goes there. A requested `reasoning_effort` moves to
/// the nearest level the model takes; GLM models before 5.2 ignore it and
/// switch thinking with `thinking.type` instead.
fn prepare_for_kind(kind: ProviderKind, model: &str, request: &mut ChatCompletionRequest) {
    if !matches!(
        kind,
        ProviderKind::Mistral | ProviderKind::Groq | ProviderKind::Zai
    ) {
        return;
    }
    for message in &mut request.messages {
        message.reasoning_details.clear();
        match kind {
            ProviderKind::Mistral => {
                message.reasoning = None;
                message.reasoning_content = None;
            }
            ProviderKind::Groq => {
                let content = message.reasoning_content.take();
                if message.reasoning.is_none() {
                    message.reasoning = content;
                }
            }
            _ => {
                let reasoning = message.reasoning.take();
                if message.reasoning_content.is_none() {
                    message.reasoning_content = reasoning;
                }
            }
        }
    }
    let levels = compatible_reasoning_efforts(kind, model);
    if let Some(effort) = request.reasoning_effort.as_mut() {
        let nearest = crate::models::nearest_effort(effort, levels).to_owned();
        *effort = nearest;
    }
    if kind == ProviderKind::Zai
        && levels == ["none", "high"]
        && let Some(effort) = request.reasoning_effort.take()
    {
        let switch = if effort == "none" {
            "disabled"
        } else {
            "enabled"
        };
        request
            .extra
            .entry("thinking".into())
            .or_insert_with(|| serde_json::json!({"type": switch}));
    }
}

fn normalize_native_openai_request(request: &mut ChatCompletionRequest) -> Result<(), RouterError> {
    if request.max_tokens.is_some() && request.max_completion_tokens.is_some() {
        return Err(RouterError::invalid(
            "max_tokens and max_completion_tokens cannot both be set",
            "max_tokens",
        ));
    }
    if request.max_completion_tokens.is_none() {
        request.max_completion_tokens = request.max_tokens.take();
    }
    if let Some(reasoning) = request.reasoning.take() {
        if reasoning.max_tokens.is_some() {
            return Err(RouterError::invalid(
                "OpenAI Chat Completions cannot preserve reasoning.max_tokens",
                "reasoning.max_tokens",
            ));
        }
        if request.reasoning_effort.is_none() {
            request.reasoning_effort = if reasoning.enabled == Some(false) {
                Some("none".into())
            } else {
                reasoning.effort
            };
        }
    }
    Ok(())
}

fn parse_usage(value: &Value) -> Usage {
    let prompt = value
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion = value
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: value
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(prompt + completion),
        prompt_tokens_details: value.get("prompt_tokens_details").cloned(),
        completion_tokens_details: value.get("completion_tokens_details").cloned(),
    }
}

fn normalize_finish(reason: &str) -> &str {
    match reason {
        "tool_use" | "function_call" => "tool_calls",
        "max_tokens" | "MAX_TOKENS" => "length",
        "STOP" => "stop",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use router_protocol::ChatCompletionRequest;

    use super::{normalize_finish, normalize_native_openai_request};
    use router_core::ProviderKind;
    use serde_json::json;

    #[test]
    fn content_chunks_split_thinking_from_text() {
        let chunks = json!([
            {"type": "thinking", "thinking": [{"type": "text", "text": "Think"}, {"type": "text", "text": "ing."}], "closed": true},
            {"type": "text", "text": "ok"},
            {"type": "thinking", "thinking": "More."},
            {"type": "image_url", "image_url": "ignored"}
        ]);
        assert_eq!(
            super::content_chunks(chunks.as_array().unwrap()),
            [
                (true, "Think".to_owned()),
                (true, "ing.".to_owned()),
                (false, "ok".to_owned()),
                (true, "More.".to_owned())
            ]
        );
    }

    #[test]
    fn mistral_and_groq_get_only_the_reasoning_fields_and_levels_they_take() {
        let request = |model: &str, effort: &str| -> ChatCompletionRequest {
            serde_json::from_value(json!({"model": model, "reasoning_effort": effort, "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "ok", "reasoning": "thought", "reasoning_content": "thought",
                 "reasoning_details": [{"type": "reasoning.text", "text": "thought"}]}
            ]})).unwrap()
        };
        let mut mistral = request("mistral-small-latest", "medium");
        super::prepare_for_kind(ProviderKind::Mistral, "mistral-small-latest", &mut mistral);
        let sent = serde_json::to_value(&mistral).unwrap();
        assert_eq!(
            sent["messages"][1],
            json!({"role": "assistant", "content": "ok"})
        );
        assert_eq!(sent["reasoning_effort"], "high");
        let mut large = request("mistral-large-4-0", "none");
        super::prepare_for_kind(ProviderKind::Mistral, "mistral-large-4-0", &mut large);
        assert_eq!(large.reasoning_effort.as_deref(), Some("low"));
        let mut groq = request("openai/gpt-oss-20b", "none");
        groq.messages[1].reasoning = None;
        super::prepare_for_kind(ProviderKind::Groq, "openai/gpt-oss-20b", &mut groq);
        let sent = serde_json::to_value(&groq).unwrap();
        assert_eq!(
            sent["messages"][1],
            json!({"role": "assistant", "content": "ok", "reasoning": "thought"})
        );
        assert_eq!(sent["reasoning_effort"], "low");
        // Z.ai continues thinking from `reasoning_content`.
        let mut glm = request("glm-5.3", "none");
        glm.messages[1].reasoning_content = None;
        super::prepare_for_kind(ProviderKind::Zai, "glm-5.3", &mut glm);
        let sent = serde_json::to_value(&glm).unwrap();
        assert_eq!(
            sent["messages"][1],
            json!({"role": "assistant", "content": "ok", "reasoning_content": "thought"})
        );
        // GLM-5.3 always thinks: `none` and `medium` move to its levels.
        assert_eq!(sent["reasoning_effort"], "low");
        let mut glm = request("glm-5.3-flash", "medium");
        super::prepare_for_kind(ProviderKind::Zai, "glm-5.3-flash", &mut glm);
        assert_eq!(glm.reasoning_effort.as_deref(), Some("high"));
        let mut glm = request("glm-5.2", "none");
        super::prepare_for_kind(ProviderKind::Zai, "glm-5.2", &mut glm);
        assert_eq!(glm.reasoning_effort.as_deref(), Some("none"));
        assert!(glm.extra.is_empty());
        // Earlier GLM models switch thinking instead of taking a level.
        for (effort, switch) in [("none", "disabled"), ("low", "enabled"), ("max", "enabled")] {
            let mut glm = request("glm-5.1", effort);
            super::prepare_for_kind(ProviderKind::Zai, "glm-5.1", &mut glm);
            let sent = serde_json::to_value(&glm).unwrap();
            assert_eq!(sent["thinking"], json!({"type": switch}), "{effort}");
            assert!(sent.get("reasoning_effort").is_none());
        }
        // Other providers get what the client sent.
        let mut deepseek = request("deepseek-v4-flash", "max");
        super::prepare_for_kind(ProviderKind::Deepseek, "deepseek-v4-flash", &mut deepseek);
        assert_eq!(
            deepseek.messages[1].reasoning_content.as_deref(),
            Some("thought")
        );
        assert_eq!(deepseek.reasoning_effort.as_deref(), Some("max"));
    }

    #[test]
    fn normalizes_provider_finish_reasons() {
        assert_eq!(normalize_finish("tool_use"), "tool_calls");
        assert_eq!(normalize_finish("max_tokens"), "length");
    }

    #[test]
    fn native_openai_maps_legacy_max_tokens_without_dropping_it() {
        let mut request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "gpt-5-nano",
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 123
        }))
        .unwrap();
        normalize_native_openai_request(&mut request).unwrap();
        assert_eq!(request.max_tokens, None);
        assert_eq!(request.max_completion_tokens, Some(123));
    }

    #[test]
    fn native_openai_rejects_ambiguous_token_limits() {
        let mut request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "gpt-5-nano",
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 123,
            "max_completion_tokens": 456
        }))
        .unwrap();
        let error = normalize_native_openai_request(&mut request).unwrap_err();
        assert_eq!(error.body.param.as_deref(), Some("max_tokens"));
    }
}
