use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use router_core::{
    Capabilities, EventStream, ProviderAdapter, ProviderConfig, ProviderEvent, RouterError,
};
use router_protocol::{ChatCompletionRequest, Usage};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{attachment::prepare_pdf_fallback, sse::request_error, transport::ProviderTransport};

pub struct OpenAiCompatibleAdapter {
    transport: ProviderTransport,
    endpoint: String,
    native_openai: bool,
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
        mut request: ChatCompletionRequest,
        capabilities: Capabilities,
        api_key: Option<&SecretString>,
        cancel: CancellationToken,
    ) -> Result<EventStream, RouterError> {
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
        let mut source = self.transport.sse(response, cancel);
        Ok(Box::pin(try_stream! {
            while let Some(value) = source.next().await {
                let value = value?;
                if let Some(error) = value.get("error") {
                    let code = error.get("code").and_then(Value::as_str).unwrap_or("upstream_stream_error");
                    Err(RouterError::upstream(
                        http::StatusCode::BAD_GATEWAY,
                        format!("upstream stream reported {code}"),
                    ))?;
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
                    if let Some(text) = delta.get("content").and_then(Value::as_str) {
                        yield ProviderEvent::TextDelta { index, text: text.into() };
                    }
                    if let Some(text) = delta
                        .get("reasoning")
                        .or_else(|| delta.get("reasoning_content"))
                        .and_then(Value::as_str)
                    {
                        yield ProviderEvent::ReasoningDelta { index, text: text.into() };
                    }
                    if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
                        for detail in details {
                            yield ProviderEvent::ReasoningDetail { index, detail: detail.clone() };
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
        }))
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
