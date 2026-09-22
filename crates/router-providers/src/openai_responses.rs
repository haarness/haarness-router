use async_stream::try_stream;
use futures::StreamExt;
use router_core::{EventStream, ProviderEvent, RouterError};
use router_protocol::{ChatCompletionRequest, MessageContent, Usage};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{sse::request_error, transport::ProviderTransport};

pub(crate) fn required(model: &str, request: &ChatCompletionRequest) -> bool {
    let version = model
        .strip_prefix("gpt-")
        .unwrap_or_default()
        .split('-')
        .next()
        .unwrap_or_default();
    let (major, minor) = version.split_once('.').unwrap_or((version, "0"));
    major.parse::<u32>().is_ok_and(|major| {
        major >= 6 || (major == 5 && minor.parse::<u32>().is_ok_and(|minor| minor >= 6))
    }) || model.contains("-codex")
        || model.ends_with("-pro")
        || model.contains("-pro-20")
        || request
            .messages
            .iter()
            .flat_map(|message| &message.reasoning_details)
            .any(|detail| detail["format"] == "openai-responses-v1")
}

pub(crate) async fn stream(
    transport: &ProviderTransport,
    http_request: reqwest::RequestBuilder,
    model: &str,
    request: &ChatCompletionRequest,
    cancel: CancellationToken,
) -> Result<EventStream, RouterError> {
    let body = responses_request(model, request)?;
    let response = http_request
        .json(&body)
        .send()
        .await
        .map_err(request_error)?;
    let response = transport.checked(response).await?;
    let mut source = transport.sse(response, cancel);
    Ok(Box::pin(try_stream! {
        let mut tool_index = 0;
        let mut completed = false;
        while let Some(event) = source.next().await {
            let event = event?;
            match event["type"].as_str().unwrap_or_default() {
                "response.output_text.delta" | "response.refusal.delta" => {
                    if let Some(text) = event["delta"].as_str() { yield ProviderEvent::TextDelta { index: 0, text: text.into() }; }
                }
                "response.reasoning_summary_text.delta" => {
                    if let Some(text) = event["delta"].as_str() { yield ProviderEvent::ReasoningDelta { index: 0, text: text.into() }; }
                }
                "response.output_item.done" => {
                    let item = &event["item"];
                    match item["type"].as_str() {
                        Some("function_call" | "custom_tool_call") => {
                            let custom = item["type"] == "custom_tool_call";
                            yield ProviderEvent::ToolCallDelta {
                                choice_index: 0, tool_index,
                                id: item["call_id"].as_str().map(str::to_owned),
                                kind: if custom { "custom" } else { "function" }.into(),
                                name: item["name"].as_str().map(str::to_owned),
                                arguments: item[if custom { "input" } else { "arguments" }].as_str().map(str::to_owned),
                            };
                            tool_index += 1;
                        }
                        Some("reasoning") if item["encrypted_content"].is_string() => {
                            yield ProviderEvent::ReasoningDetail { index: 0, detail: json!({
                                "type":"reasoning.encrypted", "format":"openai-responses-v1",
                                "id":item["id"], "data":item["encrypted_content"], "item":item
                            }) };
                        }
                        _ => {}
                    }
                }
                "response.completed" | "response.incomplete" => {
                    let response = &event["response"];
                    let usage = &response["usage"];
                    let input = usage["input_tokens"].as_u64().unwrap_or(0);
                    let output = usage["output_tokens"].as_u64().unwrap_or(0);
                    yield ProviderEvent::Usage(Usage {
                        prompt_tokens: input, completion_tokens: output,
                        total_tokens: usage["total_tokens"].as_u64().unwrap_or(input + output),
                        prompt_tokens_details: usage.get("input_tokens_details").cloned(),
                        completion_tokens_details: usage.get("output_tokens_details").cloned(),
                    });
                    let reason = if event["type"] == "response.incomplete" {
                        if response["incomplete_details"]["reason"] == "content_filter" { "content_filter" } else { "length" }
                    } else if tool_index > 0 { "tool_calls" } else { "stop" };
                    yield ProviderEvent::Finish { index: 0, reason: reason.into() };
                    completed = true;
                }
                "response.failed" | "error" => Err(RouterError::upstream(http::StatusCode::BAD_GATEWAY, "OpenAI Responses stream returned an error"))?,
                _ => {}
            }
        }
        if !completed { Err(RouterError::upstream(http::StatusCode::BAD_GATEWAY, "OpenAI Responses stream ended before completion"))?; }
    }))
}

fn responses_request(model: &str, request: &ChatCompletionRequest) -> Result<Value, RouterError> {
    // A protocol switch must never silently drop fields whose meaning cannot
    // be represented by Responses. Compatible providers keep their own wire.
    let unsupported = [
        (request.n.is_some_and(|n| n != 1), "n"),
        (request.stop.is_some(), "stop"),
        (request.seed.is_some(), "seed"),
        (request.frequency_penalty.is_some(), "frequency_penalty"),
        (request.presence_penalty.is_some(), "presence_penalty"),
        (request.logit_bias.is_some(), "logit_bias"),
        (
            request.logprobs == Some(true) || request.top_logprobs.is_some(),
            "logprobs",
        ),
        (
            request.audio.is_some() || request.modalities.is_some(),
            "audio",
        ),
        (request.prediction.is_some(), "prediction"),
        (!request.extra.is_empty(), "body"),
    ];
    if let Some((_, field)) = unsupported.into_iter().find(|(present, _)| *present) {
        return Err(RouterError::invalid(
            format!("this OpenAI model uses Responses, which cannot preserve {field}"),
            field,
        ));
    }
    let mut input = Vec::new();
    for message in &request.messages {
        if !message.extra.is_empty()
            || message.name.is_some()
            || message.function_call.is_some()
            || message.audio.is_some()
            || message.role == "function"
        {
            return Err(RouterError::invalid(
                "unsupported Responses message fields",
                "messages",
            ));
        }
        for detail in &message.reasoning_details {
            if detail["format"] != "openai-responses-v1" || detail["item"]["type"] != "reasoning" {
                return Err(RouterError::invalid(
                    "unsupported Responses reasoning history",
                    "messages.reasoning_details",
                ));
            }
            input.push(detail["item"].clone());
        }
        if message.role == "tool" {
            let call_id = message.tool_call_id.as_deref().ok_or_else(|| {
                RouterError::invalid("tool result is missing call ID", "messages.tool_call_id")
            })?;
            let custom = request
                .messages
                .iter()
                .flat_map(|m| &m.tool_calls)
                .any(|call| call.id == call_id && call.kind == "custom");
            let output = match &message.content {
                Some(MessageContent::Text(text)) => text.clone(),
                _ => {
                    return Err(RouterError::invalid(
                        "Responses tool results require text",
                        "messages.content",
                    ));
                }
            };
            input.push(json!({"type":if custom {"custom_tool_call_output"} else {"function_call_output"}, "call_id":call_id,"output":output}));
            continue;
        }
        if let Some(content) = &message.content {
            let content = match content {
                MessageContent::Text(text) => json!(text),
                MessageContent::Parts(parts) => {
                    let mut blocks = Vec::new();
                    for part in parts {
                        if !part.extra.is_empty() {
                            return Err(RouterError::invalid(
                                "unsupported Responses content fields",
                                "messages.content",
                            ));
                        }
                        blocks.push(match part.kind.as_str() {
                            "text" => json!({"type":"input_text","text":part.text.as_deref().unwrap_or_default()}),
                            "image_url" => {
                                let image = part.image_url.as_ref().ok_or_else(|| RouterError::invalid("missing image URL", "messages.content"))?;
                                let mut block = json!({"type":"input_image","image_url":image.url});
                                if let Some(detail) = &image.detail { block["detail"] = json!(detail); }
                                block
                            }
                            _ => return Err(RouterError::invalid("unsupported Responses input content", "messages.content")),
                        });
                    }
                    json!(blocks)
                }
            };
            if content != "" {
                input.push(json!({"role":message.role,"content":content}));
            }
        }
        for call in &message.tool_calls {
            if !call.extra.is_empty() {
                return Err(RouterError::invalid(
                    "unsupported Responses tool call fields",
                    "messages.tool_calls",
                ));
            }
            if let Some(function) = &call.function {
                input.push(json!({"type":"function_call","call_id":call.id,"name":function.name,"arguments":function.arguments}));
            } else if let Some(custom) = &call.custom {
                input.push(json!({"type":"custom_tool_call","call_id":call.id,"name":custom["name"],"input":custom["input"]}));
            } else {
                return Err(RouterError::invalid(
                    "missing tool call body",
                    "messages.tool_calls",
                ));
            }
        }
    }
    let mut body = json!({"model":model,"input":input,"stream":true,"store":false,"include":["reasoning.encrypted_content"]});
    let mut tools = Vec::new();
    for tool in &request.tools {
        if !tool.extra.is_empty() {
            return Err(RouterError::invalid(
                "unsupported Responses tool fields",
                "tools",
            ));
        }
        if let Some(function) = &tool.function {
            tools.push(json!({"type":"function","name":function.name,"description":function.description,"parameters":function.parameters,"strict":function.strict.unwrap_or(false)}));
        } else if let Some(custom) = &tool.custom {
            let mut tool = custom.clone();
            tool["type"] = json!("custom");
            tools.push(tool);
        }
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    if let Some(choice) = &request.tool_choice {
        body["tool_choice"] = if choice.is_string() {
            choice.clone()
        } else if let Some(name) = choice.pointer("/function/name") {
            json!({"type":"function","name":name})
        } else {
            return Err(RouterError::invalid(
                "unsupported Responses tool_choice",
                "tool_choice",
            ));
        };
    }
    if let Some(limit) = request.max_completion_tokens.or(request.max_tokens) {
        body["max_output_tokens"] = json!(limit);
    }
    if let Some(effort) = &request.reasoning_effort {
        body["reasoning"] = json!({"effort":effort,"summary":"auto"});
    }
    if let Some(value) = request.temperature {
        body["temperature"] = json!(value);
    }
    if let Some(value) = request.top_p {
        body["top_p"] = json!(value);
    }
    if let Some(value) = request.parallel_tool_calls {
        body["parallel_tool_calls"] = json!(value);
    }
    if let Some(value) = &request.service_tier {
        body["service_tier"] = json!(value);
    }
    if let Some(value) = &request.metadata {
        body["metadata"] = json!(value);
    }
    if let Some(value) = &request.user {
        body["user"] = json!(value);
    }
    if let Some(format) = &request.response_format {
        let mut format = if format["type"] == "json_schema" {
            format["json_schema"].clone()
        } else {
            format.clone()
        };
        if format.get("schema").is_some() {
            format["type"] = json!("json_schema");
        }
        body["text"] = json!({"format":format});
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_preserves_non_strict_tools_and_stateless_reasoning_history() {
        let item =
            json!({"type":"reasoning","id":"rs_1","encrypted_content":"opaque","summary":[]});
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model":"openai/gpt-6-astra", "reasoning_effort":"high", "max_completion_tokens":8192,
            "messages":[{"role":"user","content":"read"},{"role":"assistant","reasoning_details":[{"format":"openai-responses-v1","item":item}],"tool_calls":[{"type":"function","id":"call_1","function":{"name":"read","arguments":"{}"}}]},{"role":"tool","tool_call_id":"call_1","content":"proof"}],
            "tools":[{"type":"function","function":{"name":"read","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}}]
        })).unwrap();
        let body = responses_request("gpt-6-astra", &request).unwrap();
        assert_eq!(body["store"], false);
        assert_eq!(body["tools"][0]["strict"], false);
        assert_eq!(body["input"][1], item);
        assert_eq!(body["input"][2]["call_id"], body["input"][3]["call_id"]);
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["max_output_tokens"], 8192);
        assert!(required("gpt-6-astra", &request));
        assert!(required("gpt-5.6-luna", &request));
        assert!(required("gpt-7-future", &request));
    }
}
