//! AWS ConverseStream with Bedrock bearer API keys. Never consult process AWS credentials.
use crate::{sse::request_error, transport::ProviderTransport};
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use router_core::{
    Capabilities, EventStream, ProviderAdapter, ProviderConfig, ProviderEvent, RouterError,
};
use router_protocol::{ChatCompletionRequest, MessageContent, Usage};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use tokio_util::sync::CancellationToken;

pub struct BedrockAdapter {
    transport: ProviderTransport,
    base_url: String,
    idle: Duration,
}
impl BedrockAdapter {
    pub fn new(config: &ProviderConfig) -> Result<Self, RouterError> {
        Ok(Self {
            transport: ProviderTransport::new(config)?,
            base_url: config.effective_base_url()?,
            idle: Duration::from_millis(config.stream_idle_timeout_ms),
        })
    }
}
#[async_trait]
impl ProviderAdapter for BedrockAdapter {
    async fn stream(
        &self,
        model: &str,
        mut request: ChatCompletionRequest,
        capabilities: Capabilities,
        api_key: Option<&SecretString>,
        cancel: CancellationToken,
    ) -> Result<EventStream, RouterError> {
        let key = api_key.ok_or_else(|| RouterError::unavailable("Bedrock API key is missing"))?;
        crate::attachment::prepare_pdf_fallback(&mut request, capabilities).await?;
        let body = converse_request(&request)?;
        let mut url = url::Url::parse(&format!("{}/", self.base_url.trim_end_matches('/')))
            .map_err(|_| RouterError::invalid("Invalid Bedrock endpoint", "provider"))?;
        url.path_segments_mut()
            .map_err(|_| RouterError::invalid("Invalid endpoint", "provider"))?
            .pop_if_empty()
            .push("model")
            .push(model)
            .push("converse-stream");
        let response = tokio::select! {
            () = cancel.cancelled() => return Err(RouterError::timeout("request cancelled")),
            result = self.transport.client().post(url).bearer_auth(key.expose_secret()).json(&body).send() => result.map_err(request_error)?,
        };
        let response = self.transport.checked(response).await?;
        let idle = self.idle;
        Ok(Box::pin(try_stream! {
            let mut chunks = response.bytes_stream();
            let mut buffer = Vec::new();
            let mut stopped = false;
            let mut usage_seen = false;
            let mut tools = BTreeMap::new();
            loop {
                let next = tokio::select! {
                    () = cancel.cancelled() => return,
                    value = tokio::time::timeout(idle, chunks.next()) => value,
                };
                let chunk = match next {
                    Ok(Some(Ok(chunk))) => chunk,
                    Ok(Some(Err(e))) => Err(request_error(e))?,
                    Ok(None) => break,
                    Err(_) => Err(RouterError::timeout("Bedrock stream became idle"))?,
                };
                // Frames are bounded, even when transport chunks split a prelude or payload.
                for bytes in chunk.chunks(16 * 1024) {
                    buffer.extend_from_slice(bytes);
                    while let Some((kind, event)) = take_frame(&mut buffer)? {
                        let index = event["contentBlockIndex"].as_u64().unwrap_or(0) as u32;
                        match kind.as_str() {
                            "contentBlockStart" => if let Some(tool) = event.pointer("/start/toolUse") {
                                let tool_index = tools.len() as u32;
                                tools.insert(index, tool_index);
                                yield ProviderEvent::ToolCallDelta { choice_index: 0, tool_index, id: tool["toolUseId"].as_str().map(str::to_owned), kind: "function".into(), name: tool["name"].as_str().map(str::to_owned), arguments: None };
                            },
                            "contentBlockDelta" => {
                                if let Some(text) = event.pointer("/delta/text").and_then(Value::as_str) {
                                    yield ProviderEvent::TextDelta { index: 0, text: text.into() };
                                }
                                if let Some(text) = event.pointer("/delta/reasoningContent/text").and_then(Value::as_str) {
                                    yield ProviderEvent::ReasoningDelta { index: 0, text: text.into() };
                                }
                                if let Some(arguments) = event.pointer("/delta/toolUse/input").and_then(Value::as_str) {
                                    let tool_index = *tools.get(&index).ok_or_else(|| invalid_stream("Tool delta has no start"))?;
                                    yield ProviderEvent::ToolCallDelta { choice_index: 0, tool_index, id: None, kind: "function".into(), name: None, arguments: Some(arguments.into()) };
                                }
                            },
                            "messageStop" => {
                                stopped = true;
                                let reason = match event["stopReason"].as_str() { Some("tool_use") => "tool_calls", Some("max_tokens") => "length", Some("guardrail_intervened" | "content_filtered") => "content_filter", _ => "stop" };
                                yield ProviderEvent::Finish { index: 0, reason: reason.into() };
                            },
                            "metadata" => if let Some(usage) = event.get("usage") {
                                usage_seen = true;
                                let input = usage["inputTokens"].as_u64().unwrap_or(0);
                                let output = usage["outputTokens"].as_u64().unwrap_or(0);
                                let cached = usage["cacheReadInputTokens"].as_u64().unwrap_or(0);
                                let written = usage["cacheWriteInputTokens"].as_u64().unwrap_or(0);
                                // Bedrock inputTokens excludes both cache reads and cache writes.
                                let prompt = input.saturating_add(cached).saturating_add(written);
                                yield ProviderEvent::Usage(Usage { prompt_tokens: prompt, completion_tokens: output, total_tokens: prompt.saturating_add(output),
                                    prompt_tokens_details: Some(json!({"cached_tokens": cached, "cache_creation_input_tokens": written})), completion_tokens_details: None });
                            },
                            "messageStart" | "contentBlockStop" => {},
                            _ if kind.ends_with("Exception") => Err(invalid_stream("Bedrock stream returned an upstream exception"))?,
                            _ => {},
                        }
                    }
                }
            }
            if !buffer.is_empty() || !stopped || !usage_seen { Err(invalid_stream("Bedrock stream ended before completion and usage"))?; }
        }))
    }
}

fn invalid_stream(message: &str) -> RouterError {
    RouterError::upstream(http::StatusCode::BAD_GATEWAY, message)
}

/// AWS event stream: prelude + typed headers + JSON payload + CRC. Reject corruption and oversized frames.
fn take_frame(buffer: &mut Vec<u8>) -> Result<Option<(String, Value)>, RouterError> {
    if buffer.len() < 12 {
        return Ok(None);
    }
    let u32_at = |i: usize| u32::from_be_bytes(buffer[i..i + 4].try_into().unwrap());
    let len = u32_at(0) as usize;
    let headers_len = u32_at(4) as usize;
    if !(16..=1024 * 1024).contains(&len)
        || headers_len > len - 16
        || crc32fast::hash(&buffer[..8]) != u32_at(8)
    {
        return Err(invalid_stream("Invalid Bedrock event prelude"));
    }
    if buffer.len() < len {
        return Ok(None);
    }
    if crc32fast::hash(&buffer[..len - 4]) != u32_at(len - 4) {
        return Err(invalid_stream("Invalid Bedrock event checksum"));
    }
    let mut headers = &buffer[12..12 + headers_len];
    let mut kind = None;
    let mut exception = false;
    while !headers.is_empty() {
        let name_len = usize::from(headers[0]);
        if headers.len() < name_len + 4 {
            return Err(invalid_stream("Invalid Bedrock event headers"));
        }
        let name = &headers[1..1 + name_len];
        if headers[1 + name_len] != 7 {
            return Err(invalid_stream("Unsupported Bedrock event header type"));
        }
        let value_len =
            u16::from_be_bytes(headers[name_len + 2..name_len + 4].try_into().unwrap()) as usize;
        if headers.len() < name_len + 4 + value_len {
            return Err(invalid_stream("Truncated Bedrock event header"));
        }
        let value = &headers[name_len + 4..name_len + 4 + value_len];
        if name == b":event-type" {
            kind = Some(
                String::from_utf8(value.to_vec())
                    .map_err(|_| invalid_stream("Invalid Bedrock event type"))?,
            );
        }
        if name == b":message-type" && value != b"event" {
            exception = true;
        }
        headers = &headers[name_len + 4 + value_len..];
    }
    if exception {
        return Err(invalid_stream(
            "Bedrock stream returned an upstream exception",
        ));
    }
    let kind = kind.ok_or_else(|| invalid_stream("Missing Bedrock event type"))?;
    let payload = serde_json::from_slice(&buffer[12 + headers_len..len - 4])
        .map_err(|_| invalid_stream("Invalid Bedrock event JSON"))?;
    buffer.drain(..len);
    Ok(Some((kind, payload)))
}

fn converse_request(request: &ChatCompletionRequest) -> Result<Value, RouterError> {
    // Never silently discard unsupported request semantics.
    if request.response_format.is_some()
        || request.reasoning.is_some()
        || request.reasoning_effort.is_some()
        || request.seed.is_some()
        || request.logprobs.is_some()
        || request.top_logprobs.is_some()
        || request.frequency_penalty.is_some()
        || request.presence_penalty.is_some()
        || request.logit_bias.is_some()
        || request.audio.is_some()
        || request.modalities.is_some()
        || request.prediction.is_some()
        || request.service_tier.is_some()
        || request.n.is_some_and(|n| n != 1)
        || !request.extra.is_empty()
    {
        return Err(RouterError::invalid(
            "Unsupported Bedrock Converse parameter",
            "body",
        ));
    }
    let mut system = Vec::new();
    let mut messages = Vec::<Value>::new();
    for message in &request.messages {
        if !message.reasoning_details.is_empty()
            || message.function_call.is_some()
            || !message.extra.is_empty()
        {
            return Err(RouterError::invalid(
                "Unsupported Bedrock message parameter",
                "messages",
            ));
        }
        let mut content = match &message.content {
            None => vec![],
            Some(MessageContent::Text(text)) => {
                if text.is_empty() {
                    vec![]
                } else {
                    vec![json!({"text": text})]
                }
            }
            Some(MessageContent::Parts(parts)) => parts
                .iter()
                .map(|part| {
                    if part.kind == "text" {
                        Ok(json!({"text":part.text.as_deref().unwrap_or("")}))
                    } else {
                        Err(RouterError::invalid(
                            "Bedrock currently accepts text and tool messages",
                            "messages",
                        ))
                    }
                })
                .collect::<Result<Vec<_>, _>>()?,
        };
        if matches!(message.role.as_str(), "system" | "developer") {
            system.extend(content);
            continue;
        }
        let role = match message.role.as_str() {
            "assistant" => "assistant",
            "user" | "tool" => "user",
            _ => return Err(RouterError::invalid("Unsupported Bedrock role", "messages")),
        };
        if message.role == "tool" {
            let id = message
                .tool_call_id
                .as_deref()
                .ok_or_else(|| RouterError::invalid("tool_call_id is required", "messages"))?;
            content = vec![json!({"toolResult":{"toolUseId":id,"content":content}})];
        }
        for call in &message.tool_calls {
            let function = call
                .function
                .as_ref()
                .ok_or_else(|| RouterError::invalid("Bedrock requires function tools", "tools"))?;
            let input: Value = serde_json::from_str(&function.arguments)
                .map_err(|_| RouterError::invalid("Tool arguments must be JSON", "messages"))?;
            content
                .push(json!({"toolUse":{"toolUseId":call.id,"name":function.name,"input":input}}));
        }
        if let Some(last) = messages.last_mut().filter(|m| m["role"] == role) {
            last["content"].as_array_mut().unwrap().extend(content);
        } else {
            messages.push(json!({"role":role,"content":content}));
        }
    }
    let mut inference =
        json!({"maxTokens":request.max_completion_tokens.or(request.max_tokens).unwrap_or(4096)});
    if let Some(v) = request.temperature {
        inference["temperature"] = json!(v);
    }
    if let Some(v) = request.top_p {
        inference["topP"] = json!(v);
    }
    if let Some(v) = &request.stop {
        inference["stopSequences"] = if v.is_string() { json!([v]) } else { v.clone() };
    }
    let mut body = json!({"messages":messages,"inferenceConfig":inference});
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    if !request.tools.is_empty() {
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                let f = tool.function.as_ref().ok_or_else(|| {
                    RouterError::invalid("Bedrock requires function tools", "tools")
                })?;
                let mut spec = json!({"name":f.name,"inputSchema":{"json":f.parameters}});
                if let Some(description) = &f.description {
                    spec["description"] = json!(description);
                }
                Ok(json!({"toolSpec":spec}))
            })
            .collect::<Result<Vec<_>, RouterError>>()?;
        body["toolConfig"] = json!({"tools":tools});
        if let Some(choice) = &request.tool_choice {
            body["toolConfig"]["toolChoice"] = match choice.as_str() {
                Some("auto") => json!({"auto":{}}),
                Some("required") => json!({"any":{}}),
                Some("none") => {
                    body.as_object_mut().unwrap().remove("toolConfig");
                    return Ok(body);
                }
                _ => {
                    json!({"tool":{"name":choice.pointer("/function/name").and_then(Value::as_str).ok_or_else(|| RouterError::invalid("Invalid tool choice", "tool_choice"))?}})
                }
            };
        }
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(kind: &str, value: Value) -> Vec<u8> {
        let mut headers = Vec::new();
        for (name, value) in [
            (":message-type", "event"),
            (":event-type", kind),
            (":content-type", "application/json"),
        ] {
            headers.push(name.len() as u8);
            headers.extend(name.as_bytes());
            headers.push(7);
            headers.extend((value.len() as u16).to_be_bytes());
            headers.extend(value.as_bytes());
        }
        let payload = value.to_string();
        let mut result = Vec::new();
        result.extend(((16 + headers.len() + payload.len()) as u32).to_be_bytes());
        result.extend((headers.len() as u32).to_be_bytes());
        result.extend(crc32fast::hash(&result).to_be_bytes());
        result.extend(headers);
        result.extend(payload.as_bytes());
        result.extend(crc32fast::hash(&result).to_be_bytes());
        result
    }
    #[test]
    fn bounded_frames_accept_arbitrary_splits_and_reject_corruption() {
        let wire = frame("contentBlockDelta", json!({"delta":{"text":"hello"}}));
        for split in 0..wire.len() {
            let mut buffer = wire[..split].to_vec();
            assert!(take_frame(&mut buffer).unwrap().is_none());
            buffer.extend(&wire[split..]);
            assert_eq!(
                take_frame(&mut buffer).unwrap().unwrap().1["delta"]["text"],
                "hello"
            );
            assert!(buffer.is_empty());
        }
        let mut corrupt = wire.clone();
        corrupt[20] ^= 1;
        assert!(take_frame(&mut corrupt).is_err());
        let mut oversized = vec![0xff; 12];
        assert!(take_frame(&mut oversized).is_err());
    }
    #[test]
    fn tools_and_results_round_trip_without_losing_system_prompt() {
        let request = serde_json::from_value(json!({"model":"amazon-bedrock/test","messages":[{"role":"system","content":"Be brief"},{"role":"user","content":"weather"},{"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Paris\"}"}}]},{"role":"tool","tool_call_id":"c1","content":"sunny"}],"tools":[{"type":"function","function":{"name":"weather","parameters":{"type":"object"}}}],"tool_choice":"required"})).unwrap();
        let body = converse_request(&request).unwrap();
        assert_eq!(body["system"][0]["text"], "Be brief");
        assert_eq!(
            body["messages"][1]["content"][0]["toolUse"]["input"]["city"],
            "Paris"
        );
        assert_eq!(
            body["messages"][2]["content"][0]["toolResult"]["toolUseId"],
            "c1"
        );
        assert_eq!(body["toolConfig"]["toolChoice"], json!({"any":{}}));
    }
    #[tokio::test]
    async fn real_http_stream_uses_bearer_and_converse_and_preserves_tools_usage() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut input = Vec::new();
            loop {
                let mut bytes = [0; 1024];
                let count = socket.read(&mut bytes).await.unwrap();
                assert!(count > 0);
                input.extend_from_slice(&bytes[..count]);
                if let Some(end) = input.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&input[..end]);
                    let length = head
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|v| v.parse::<usize>().ok())
                        })
                        .unwrap();
                    if input.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let wire = String::from_utf8_lossy(&input);
            assert!(wire.starts_with("POST /model/us.anthropic.test:0/converse-stream "));
            assert!(wire.contains("authorization: Bearer customer-only-key"));
            assert!(wire.contains("\"inferenceConfig\""));
            let frames = [
                frame("messageStart",json!({"role":"assistant"})),
                frame("contentBlockDelta",json!({"contentBlockIndex":0,"delta":{"text":"Checking"}})),
                frame("contentBlockStart",json!({"contentBlockIndex":1,"start":{"toolUse":{"toolUseId":"call_1","name":"weather"}}})),
                frame("contentBlockDelta",json!({"contentBlockIndex":1,"delta":{"toolUse":{"input":"{\"city\":\"Paris\"}"}}})),
                frame("messageStop",json!({"stopReason":"tool_use"})),
                frame("metadata",json!({"usage":{"inputTokens":10,"outputTokens":4,"cacheReadInputTokens":2,"cacheWriteInputTokens":3}})),
            ].concat();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.amazon.eventstream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                frames.len()
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            for part in frames.chunks(7) {
                socket.write_all(part).await.unwrap();
                tokio::task::yield_now().await;
            }
        });
        let config:ProviderConfig=serde_json::from_value(json!({"kind":"amazon-bedrock","base_url":format!("http://{address}"),"allow_insecure_http":true})).unwrap();
        let adapter = BedrockAdapter::new(&config).unwrap();
        let request=serde_json::from_value(json!({"model":"amazon-bedrock/us.anthropic.test:0","messages":[{"role":"user","content":"weather"}],"max_tokens":30})).unwrap();
        let key = SecretString::from("customer-only-key");
        let mut stream = adapter
            .stream(
                "us.anthropic.test:0",
                request,
                config.effective_capabilities(),
                Some(&key),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.unwrap());
        }
        server.await.unwrap();
        assert!(matches!(&events[0], ProviderEvent::TextDelta {text,..} if text=="Checking"));
        assert!(
            events.iter().any(
                |e| matches!(e, ProviderEvent::ToolCallDelta {name:Some(n),..} if n=="weather")
            )
        );
        assert!(events.iter().any(|e| matches!(e, ProviderEvent::Usage(u) if u.prompt_tokens==15 && u.completion_tokens==4 && u.total_tokens==19 && u.prompt_tokens_details.as_ref().unwrap()["cache_creation_input_tokens"]==3)));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ProviderEvent::Finish {reason,..} if reason=="tool_calls"))
        );
    }
}
