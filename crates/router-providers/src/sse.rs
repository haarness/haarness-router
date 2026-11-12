use std::{pin::Pin, time::Duration};

use async_stream::try_stream;
use futures::{Stream, StreamExt};
use router_core::{RouterError, UpstreamFailure, is_safe_request_id};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

const MAX_SSE_FRAME_BYTES: usize = 1024 * 1024;

/// Response headers that carry a provider's own request ID, in lookup order:
/// the OpenAI-compatible family, Anthropic, then Amazon Bedrock. Gemini sets
/// none of them unless a proxy in front of it does.
const UPSTREAM_REQUEST_ID_HEADERS: [&str; 3] = ["x-request-id", "request-id", "x-amzn-requestid"];

pub type JsonSseStream = Pin<Box<dyn Stream<Item = Result<Value, RouterError>> + Send + 'static>>;

pub fn json_sse_stream(
    response: reqwest::Response,
    cancel: CancellationToken,
    idle_timeout: Duration,
) -> JsonSseStream {
    Box::pin(try_stream! {
        let mut chunks = response.bytes_stream();
        let mut buffer = Vec::with_capacity(8 * 1024);
        loop {
            let next = tokio::select! {
                () = cancel.cancelled() => return,
                result = tokio::time::timeout(idle_timeout, chunks.next()) => result,
            };
            let chunk = match next {
                Ok(Some(Ok(chunk))) => chunk,
                Ok(Some(Err(error))) => Err(RouterError::transport(
                    format!("upstream stream failed: {error}"),
                    true,
                ))?,
                Ok(None) => break,
                Err(_) => Err(RouterError::timeout("upstream stream became idle")
                    .with_upstream_failure(UpstreamFailure::Timeout))?,
            };
            buffer.extend_from_slice(&chunk);
            if buffer.len() > MAX_SSE_FRAME_BYTES && find_boundary(&buffer).is_none() {
                Err(RouterError::upstream(
                    http::StatusCode::BAD_GATEWAY,
                    "upstream SSE frame exceeded the 1 MiB limit",
                ))?;
            }
            while let Some((index, delimiter)) = find_boundary(&buffer) {
                let frame = buffer.drain(..index).collect::<Vec<_>>();
                buffer.drain(..delimiter);
                let mut data = String::new();
                for line in String::from_utf8_lossy(&frame).lines() {
                    let line = line.trim_end_matches('\r');
                    if line.starts_with(':') {
                        continue;
                    }
                    if let Some(value) = line.strip_prefix("data:") {
                        if !data.is_empty() {
                            data.push('\n');
                        }
                        data.push_str(value.trim_start());
                    }
                }
                if data.is_empty() {
                    continue;
                }
                if data == "[DONE]" {
                    return;
                }
                let value = serde_json::from_str(&data).map_err(|error| {
                    RouterError::upstream(
                        http::StatusCode::BAD_GATEWAY,
                        format!("upstream returned invalid SSE JSON: {error}"),
                    )
                })?;
                yield value;
            }
        }
        if !buffer.iter().all(u8::is_ascii_whitespace) {
            Err(RouterError::upstream(
                http::StatusCode::BAD_GATEWAY,
                "upstream ended with an incomplete SSE frame",
            ))?;
        }
    })
}

pub async fn checked_response(
    mut response: reqwest::Response,
    max_error_body_bytes: usize,
) -> Result<reqwest::Response, RouterError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let upstream_request_id = upstream_request_id(response.headers());
    // Inspect only a bounded machine-readable error type/code. Never expose the
    // arbitrary upstream message or raw body: either can contain echoed prompts,
    // credentials, or provider-specific internals.
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok().flatten() {
        if bytes.len().saturating_add(chunk.len()) > max_error_body_bytes {
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    let retry_after = response_retry_after(response.headers());
    let detail = router_core::detail::from_error_body(&bytes);
    if out_of_credit(status, &String::from_utf8_lossy(&bytes)) {
        return Err(out_of_credit_error()
            .with_upstream_failure(UpstreamFailure::from_status(status))
            .with_upstream_detail(detail)
            .with_upstream_request_id(upstream_request_id));
    }
    let upstream_code = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/type")
                .or_else(|| value.pointer("/error/code"))
                .or_else(|| value.get("type"))
                .or_else(|| value.get("code"))
                .and_then(Value::as_str)
                .map(sanitize_code)
        })
        .filter(|value| !value.is_empty());
    let (status, message) = plain_refusal(status, &String::from_utf8_lossy(&bytes), retry_after)
        .unwrap_or_else(|| {
            let detail = upstream_code.map_or_else(String::new, |code| format!(" ({code})"));
            (
                status,
                format!("upstream returned HTTP {}{detail}", status.as_u16()),
            )
        });
    let mut error = RouterError::upstream(status, message)
        .with_upstream_failure(UpstreamFailure::from_status(status))
        .with_upstream_detail(detail)
        .with_upstream_request_id(upstream_request_id);
    // A wait helps only when the provider is busy, not with a request it will never take.
    error.retry_after_secs = retry_after
        .filter(|_| matches!(status.as_u16(), 429 | 503))
        .and_then(|wait| {
            std::num::NonZeroU16::new(u16::try_from(wait.as_secs().max(1)).unwrap_or(u16::MAX))
        });
    Err(error)
}

/// What a key's owner reads when the provider account behind the key has no
/// credit left. A router refused on its own key says so in its own words
/// (`Router` rewrites this).
pub const OUT_OF_CREDIT: &str = "The provider account behind this key has run out of credit, so the provider refused the request. Add credit with the provider, or use another key or provider.";

fn out_of_credit_error() -> RouterError {
    RouterError::new(
        http::StatusCode::PAYMENT_REQUIRED,
        OUT_OF_CREDIT,
        "upstream_error",
        router_core::PROVIDER_OUT_OF_CREDIT,
    )
}

/// Whether a provider refused because the account behind the key has no
/// credit, or too little for the request. Providers say it with different
/// statuses: Anthropic 400, `OpenAI` 429 (`insufficient_quota`), `xAI` 403,
/// `OpenRouter` and `DeepSeek` 402. Waiting never clears it.
fn out_of_credit(status: http::StatusCode, body: &str) -> bool {
    if status == http::StatusCode::PAYMENT_REQUIRED {
        return true;
    }
    if !matches!(status.as_u16(), 400 | 403 | 429) {
        return false;
    }
    let lower = body.to_ascii_lowercase();
    [
        "credit balance is too low",
        "insufficient_quota",
        "insufficient balance",
        "insufficient credits",
        "used all available credits",
        "requires more credits",
    ]
    .iter()
    .any(|words| lower.contains(words))
}

/// An error event a provider sent inside its stream (`{"error": {...}}`):
/// `OpenRouter` sends the serving provider's HTTP status as a number, others a
/// string code. A refusal keeps its status, so a rate limit reads and backs
/// off as one and a request the provider will never take is not tried again.
pub fn stream_error(event: &Value) -> RouterError {
    let error = event.get("error").unwrap_or(event);
    let detail = router_core::detail::from_error_value(event);
    let numeric = error
        .get("code")
        .and_then(|code| {
            code.as_u64()
                .or_else(|| code.as_str().and_then(|code| code.parse().ok()))
        })
        .and_then(|code| u16::try_from(code).ok())
        .and_then(|code| http::StatusCode::from_u16(code).ok())
        .filter(|status| status.is_client_error() || status.is_server_error());
    let named = error
        .get("code")
        .and_then(Value::as_str)
        .filter(|code| code.parse::<u16>().is_err())
        .map(sanitize_code)
        .filter(|code| !code.is_empty());
    let raw = event.to_string();
    let status = numeric.unwrap_or(if named.as_deref() == Some("rate_limit_exceeded") {
        http::StatusCode::TOO_MANY_REQUESTS
    } else {
        http::StatusCode::BAD_GATEWAY
    });
    if out_of_credit(status, &raw) || named.as_deref() == Some("insufficient_quota") {
        return out_of_credit_error()
            .with_upstream_failure(UpstreamFailure::Other)
            .with_upstream_detail(detail);
    }
    // A request the provider refused for what it holds reads as it does when
    // refused with an HTTP status. OpenRouter sends that status as a number
    // once the stream has begun; a named code is taken as a bad request, not a
    // busy server, so it isn't tried again.
    let as_request = match numeric {
        Some(status) if matches!(status.as_u16(), 400 | 403 | 413) => Some(status),
        None if status != http::StatusCode::TOO_MANY_REQUESTS => {
            Some(http::StatusCode::BAD_REQUEST)
        }
        _ => None,
    };
    if let Some((status, message)) = as_request.and_then(|status| {
        request_refusal(status, &raw.to_ascii_lowercase()).map(|message| (status, message))
    }) {
        return RouterError::upstream(status, message)
            .with_upstream_failure(UpstreamFailure::Other)
            .with_upstream_detail(detail);
    }
    let (status, message) = match (status.as_u16(), named) {
        (429, _) => plain_refusal(status, &raw, None)
            .unwrap_or_else(|| (status, "upstream stream reported a rate limit".to_owned())),
        (_, Some(code)) => (status, format!("upstream stream reported {code}")),
        _ if numeric.is_some() => (
            status,
            format!("upstream stream reported HTTP {}", status.as_u16()),
        ),
        _ => (
            status,
            "upstream stream reported upstream_stream_error".to_owned(),
        ),
    };
    RouterError::upstream(status, message)
        .with_upstream_failure(UpstreamFailure::Other)
        .with_upstream_detail(detail)
}

/// A provider's `Retry-After` in seconds, up to a day. An HTTP date is not read.
fn response_retry_after(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    let seconds = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)?;
    Some(std::time::Duration::from_secs_f64(seconds.min(86_400.0)))
}

/// The router's own words for refusals a caller can act on. The provider's
/// message is read to tell them apart and for its numbers only, never passed
/// on: it can echo prompts, account IDs or credentials.
fn plain_refusal(
    status: http::StatusCode,
    body: &str,
    retry_after: Option<std::time::Duration>,
) -> Option<(http::StatusCode, String)> {
    let lower = body.to_ascii_lowercase();
    // A request larger than the plan's tokens a minute (Groq's free tier
    // allows 8,000) is refused with 413 or 429; waiting never clears it.
    if matches!(status.as_u16(), 413 | 429)
        && lower.contains("tokens per minute")
        && (lower.contains("request too large") || lower.contains("reduce your message size"))
    {
        let number = |label: &str| {
            lower
                .split_once(label)
                .and_then(|(_, rest)| rest.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|digits| digits.parse::<u64>().ok())
        };
        let message = match (number("limit "), number("requested ")) {
            (Some(limit), Some(requested)) => format!(
                "This request needs {requested} tokens, more than the {limit} tokens a minute the provider's plan allows, so the provider refused it. Send a shorter request, raise the plan's limit, or use another provider."
            ),
            _ => "This request is larger than the provider's plan allows in a minute, so the provider refused it. Send a shorter request, raise the plan's limit, or use another provider.".to_owned(),
        };
        return Some((http::StatusCode::PAYLOAD_TOO_LARGE, message));
    }
    if let Some(message) = request_refusal(status, &lower) {
        return Some((status, message));
    }
    match status.as_u16() {
        401 | 403 if lower.contains("api keys are not supported") => Some((
            status,
            "Google Vertex AI takes only API keys bound to a service account, and this key is not one. A key from Google AI Studio works with the Gemini provider instead.".to_owned(),
        )),
        401 => Some((
            status,
            "The provider refused the API key. Check the key, or replace it.".to_owned(),
        )),
        403 => Some((
            status,
            "The provider refused this request for this API key. Check what the key is allowed to use.".to_owned(),
        )),
        429 => Some((
            status,
            match retry_after {
                Some(wait) => match wait.as_secs().max(1) {
                    1 => "The provider is rate limiting this key. Try again in 1 second.".to_owned(),
                    seconds => format!(
                        "The provider is rate limiting this key. Try again in {seconds} seconds."
                    ),
                },
                None => "The provider is rate limiting this key. Try again shortly.".to_owned(),
            },
        )),
        _ => None,
    }
}

/// What a caller reads when the provider's content check flagged the request.
pub const CONTENT_FLAGGED: &str = "The model's provider flagged this conversation in its content check and refused it. Rephrase your last message, start a new chat if something earlier in this one set it off, or choose another model.";

/// A request the provider will never take as it stands, refused for what it
/// holds rather than for the key or the load: media a text-only model can't
/// read, a content check, a prompt longer than the model's window. Read from
/// the lower-cased error body; the same words whether the refusal came as an
/// HTTP status or inside a stream.
fn request_refusal(status: http::StatusCode, lower: &str) -> Option<String> {
    // A text-only model sent an image or other media (Z.ai: "messages.content.type
    // is invalid, allowed values: ['text']").
    if status.as_u16() == 400
        && lower.contains("content.type is invalid")
        && lower.contains("['text']")
    {
        return Some("This model reads only text. Send the request without the image or file, or choose a model that reads them.".to_owned());
    }
    // A content check (Alibaba's data_inspection_failed for Qwen, "Input data
    // may contain inappropriate content"; OpenAI's invalid_prompt; Azure's
    // content_filter; OpenRouter's moderation, a 403 "your input was
    // flagged"): sending it again is refused again.
    if matches!(status.as_u16(), 400 | 403)
        && [
            "data_inspection_failed",
            "datainspectionfailed",
            "may contain inappropriate content",
            "content_policy_violation",
            "invalid_prompt",
            "responsibleaipolicyviolation",
            "content_filter",
            "was flagged",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return Some(CONTENT_FLAGGED.to_owned());
    }
    // A prompt longer than the model's window ("This endpoint's maximum context
    // length is 16384 tokens", OpenAI's context_length_exceeded, Claude's
    // "prompt is too long"): a client compacts the conversation on these words,
    // which "upstream returned HTTP 400" never gave it. The window the provider
    // names is said as "maximum context length is N tokens", which clients
    // read to compact once to the right size.
    if matches!(status.as_u16(), 400 | 413) && context_overflow(lower) {
        return Some(match named_window(lower) {
            Some(window) => format!(
                "The conversation is longer than this model's maximum context length, so the provider refused it (its maximum context length is {window} tokens). Compact the conversation, or choose a model with a larger context window."
            ),
            None => "The conversation is longer than this model's maximum context length, so the provider refused it. Compact the conversation, or choose a model with a larger context window.".to_owned(),
        });
    }
    None
}

/// A refusal inside a stream of a request the provider will never take as
/// it stands (see [`request_refusal`]), read from the event's `error` only:
/// `None` for any other failure, which keeps the stream's own words.
#[must_use]
pub fn stream_request_refusal(failure: &Value) -> Option<RouterError> {
    // OpenAI's `error` event carries its code at the top; a failed response
    // its `error`. The rest of a response (its instructions, its output) is
    // never read.
    let error = failure.get("error").or_else(|| {
        (failure.get("type").and_then(Value::as_str) == Some("error")).then_some(failure)
    })?;
    let message = request_refusal(
        http::StatusCode::BAD_REQUEST,
        &error.to_string().to_ascii_lowercase(),
    )?;
    Some(
        RouterError::upstream(http::StatusCode::BAD_REQUEST, message)
            .with_upstream_failure(UpstreamFailure::Other)
            .with_upstream_detail(router_core::detail::from_error_value(failure)),
    )
}

/// A provider's words for a prompt longer than the model's window, as the
/// servers behind the router say them: `OpenAI`, `OpenRouter`, vLLM and
/// `DeepSeek` ("maximum context length is 32768 tokens"), Claude ("prompt is
/// too long"), `DeepInfra` ("should not exceed `max_num_tokens` (32768)",
/// "exceeds maximum input length 131071"), `OpenRouter` past every endpoint
/// ("longer than the model's max input length (131072 tokens)"), llama.cpp,
/// `xAI`, Moonshot, Qwen on `DashScope`, Gemini and Amazon Nova. An output limit above the model's
/// ("`max_tokens` must be less than") and Z.ai's image count are no window:
/// compacting can't answer them.
fn context_overflow(lower: &str) -> bool {
    if lower.contains("max_tokens must be less than")
        || lower.contains("image count")
        || lower.contains("图片数")
    {
        return false;
    }
    [
        "context_length_exceeded",
        "maximum context length",
        "context length exceeded",
        "context window exceeded",
        "exceeds the context window",
        "model_context_window_exceeded",
        "prompt is too long",
        "input is too long",
        "prompt too long",
        "should not exceed max_num_tokens",
        "exceeds maximum input length",
        "longer than the model's max input length",
        "longer than the model's context length",
        "exceeds the available context size",
        "greater than the context length",
        "longer than the maximum model length",
        "context length is only",
        "maximum prompt length is",
        "exceeded model token limit",
        "reduce the length of the messages",
        "range of input length should be",
        "exceeds the maximum number of tokens",
        "number of input tokens exceeds maximum length",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

/// The context window a refusal of a prompt as too long names: the least
/// number after one of the providers' phrasings ("maximum context length is
/// 32768 tokens", "`max_num_tokens` (32768)", "200000 maximum" after Claude's
/// "tokens > "). A count of the refused request is never taken.
fn named_window(lower: &str) -> Option<u64> {
    const BEFORE: [&str; 12] = [
        "maximum context length is ",
        "max_num_tokens (",
        "max input length (",
        "maximum input length ",
        "available context size (",
        "context length is only ",
        "maximum prompt length is ",
        "model token limit: ",
        "input length should be [1, ",
        "maximum number of tokens allowed (",
        "maximum model length of ",
        " tokens > ",
    ];
    BEFORE
        .iter()
        .filter_map(|before| {
            let (_, rest) = lower.split_once(before)?;
            let digits: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == ',')
                .filter(char::is_ascii_digit)
                .collect();
            digits.parse::<u64>().ok().filter(|window| *window >= 1_024)
        })
        .min()
}

/// The provider's own request ID, when one is present and safe to log.
pub fn upstream_request_id(headers: &reqwest::header::HeaderMap) -> Option<String> {
    UPSTREAM_REQUEST_ID_HEADERS.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .filter(|value| is_safe_request_id(value))
            .map(str::to_owned)
    })
}

pub fn request_error(error: reqwest::Error) -> RouterError {
    let (safe, failure) = if error.is_connect() {
        ("failed to connect to upstream", UpstreamFailure::Connect)
    } else if error.is_timeout() {
        ("upstream request timed out", UpstreamFailure::Timeout)
    } else {
        ("upstream request failed", UpstreamFailure::Other)
    };
    RouterError::transport(safe, !error.is_connect()).with_upstream_failure(failure)
}

fn find_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let crlf = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4));
    let lf = buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2));
    match (crlf, lf) {
        (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn sanitize_code(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
        .take(64)
        .collect()
}

#[cfg(test)]
mod tests {
    use reqwest::header::{HeaderMap, HeaderValue};

    use super::{find_boundary, plain_refusal, upstream_request_id};
    use std::time::Duration;

    #[test]
    fn refusals_a_caller_can_act_on_read_plainly_and_echo_nothing() {
        let groq = r#"{"error":{"message":"Request too large for model `openai/gpt-oss-20b` in organization `org_secret` service tier `on_demand` on tokens per minute (TPM): Limit 8000, Requested 12082, please reduce your message size and try again.","type":"tokens","code":"rate_limit_exceeded"}}"#;
        for status in [413, 429] {
            let (status, message) =
                plain_refusal(http::StatusCode::from_u16(status).unwrap(), groq, None).unwrap();
            assert_eq!(status, http::StatusCode::PAYLOAD_TOO_LARGE);
            assert!(
                message.starts_with(
                    "This request needs 12082 tokens, more than the 8000 tokens a minute"
                ),
                "{message}"
            );
            assert!(!message.contains("org_secret"));
        }
        let vertex = r#"{"error":{"code":401,"message":"API keys are not supported by this API. Expected OAuth2 access token","status":"UNAUTHENTICATED"}}"#;
        assert!(
            plain_refusal(http::StatusCode::UNAUTHORIZED, vertex, None)
                .unwrap()
                .1
                .starts_with("Google Vertex AI takes only API keys bound to a service account")
        );
        let mistral = r#"{"detail":"Invalid API Key"}"#;
        assert_eq!(
            plain_refusal(http::StatusCode::UNAUTHORIZED, mistral, None)
                .unwrap()
                .1,
            "The provider refused the API key. Check the key, or replace it."
        );
        assert_eq!(
            plain_refusal(
                http::StatusCode::TOO_MANY_REQUESTS,
                r#"{"message":"Requests rate limit exceeded"}"#,
                Some(Duration::from_secs(31))
            )
            .unwrap(),
            (
                http::StatusCode::TOO_MANY_REQUESTS,
                "The provider is rate limiting this key. Try again in 31 seconds.".to_owned()
            )
        );
        let zai = r#"{"error":{"code":"1214","message":"messages.content.type is invalid, allowed values: ['text']"}}"#;
        assert!(
            plain_refusal(http::StatusCode::BAD_REQUEST, zai, None)
                .unwrap()
                .1
                .starts_with("This model reads only text.")
        );
        assert!(plain_refusal(http::StatusCode::NOT_FOUND, "{}", None).is_none());
        assert!(plain_refusal(http::StatusCode::BAD_REQUEST, groq, None).is_none());
        assert_eq!(
            plain_refusal(
                http::StatusCode::TOO_MANY_REQUESTS,
                "{}",
                Some(Duration::from_millis(400))
            )
            .unwrap()
            .1,
            "The provider is rate limiting this key. Try again in 1 second."
        );
    }

    #[test]
    fn a_prompt_longer_than_the_window_says_so_in_words_a_client_compacts_on() {
        let openrouter = r#"{"error":{"message":"This endpoint's maximum context length is 32768 tokens. However, you requested about 41797 tokens (337 of text input, 15246 of tool input, 26214 in the output). Please reduce the length of either one, or use the context-compression plugin to compress your prompt automatically.","code":400}}"#;
        let openai = r#"{"error":{"message":"Your input exceeds the context window of this model.","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
        let claude = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#;
        let deepinfra = r#"{"error":{"message":"The sum of prompt length (66478.0), query length (0) should not exceed max_num_tokens (32768)","code":400}}"#;
        for (body, window) in [
            (openrouter, Some("32768")),
            (openai, None),
            (claude, Some("200000")),
            (deepinfra, Some("32768")),
        ] {
            let (status, message) =
                plain_refusal(http::StatusCode::BAD_REQUEST, body, None).expect(body);
            assert_eq!(status, http::StatusCode::BAD_REQUEST);
            assert!(message.contains("maximum context length"), "{message}");
            // The window is said as clients read it; the refused request's
            // own counts never are.
            match window {
                Some(window) => assert!(
                    message.contains(&format!("maximum context length is {window} tokens")),
                    "{message}"
                ),
                None => assert!(!message.chars().any(|c| c.is_ascii_digit()), "{message}"),
            }
            for count in ["41797", "210000", "66478"] {
                assert!(!message.contains(count), "{message}");
            }
        }
        // An output limit above the model's and Z.ai's image count are no
        // window: compacting can't answer them.
        for body in [
            r#"{"message":"max_tokens must be less than or equal to 4096, too many tokens"}"#,
            r#"{"error":{"code":"1261","message":"Prompt too long; exceeded max image count"}}"#,
        ] {
            assert!(
                plain_refusal(http::StatusCode::BAD_REQUEST, body, None).is_none(),
                "{body}"
            );
        }
        // Another 400 keeps the generic status line.
        assert!(
            plain_refusal(
                http::StatusCode::BAD_REQUEST,
                r#"{"error":{"message":"messages.1.content: unexpected field"}}"#,
                None
            )
            .is_none()
        );
    }

    #[test]
    fn a_request_refused_inside_a_stream_reads_as_one_refused_with_a_status() {
        use super::{CONTENT_FLAGGED, stream_error, stream_request_refusal};
        // DeepInfra serving deepseek/deepseek-chat behind OpenRouter, after the
        // stream began: a client must compact, and learn the 32K window.
        let long = serde_json::json!({"id":"gen-1","object":"chat.completion.chunk","model":"deepseek/deepseek-chat","provider":"DeepInfra","choices":[],"error":{"code":400,"message":"Upstream error from DeepInfra: The sum of prompt length (66478.0), query length (0) should not exceed max_num_tokens (32768)","metadata":{"error_type":"invalid_request"}}});
        let error = stream_error(&long);
        assert_eq!(error.status, http::StatusCode::BAD_REQUEST);
        assert!(!error.retryable);
        // OpenRouter itself, for a prompt past every endpoint's input limit.
        let past_all = serde_json::json!({"error":{"code":413,"message":"The input (137016 tokens) is longer than the model's max input length (131072 tokens)."}});
        let refused = stream_error(&past_all);
        assert_eq!(refused.status, http::StatusCode::PAYLOAD_TOO_LARGE);
        assert!(
            refused
                .body
                .message
                .contains("maximum context length is 131072 tokens"),
            "{}",
            refused.body.message
        );
        assert!(!refused.body.message.contains("137016"));
        assert!(
            error
                .body
                .message
                .contains("maximum context length is 32768 tokens"),
            "{}",
            error.body.message
        );
        assert!(!error.body.message.contains("66478"));
        // Alibaba's content check on Qwen, its SSE line passed on by OpenRouter.
        let flagged = serde_json::json!({"error":{"code":400,"message":"Provider returned error","metadata":{"raw":"data: {\"error\":{\"code\":\"data_inspection_failed\",\"param\":null,\"message\":\"Input data may contain inappropriate content.\",\"type\":\"data_inspection_failed\"},\"id\":\"chatcmpl-1\"}","provider_name":"Alibaba"}}});
        let error = stream_error(&flagged);
        assert_eq!(error.status, http::StatusCode::BAD_REQUEST);
        assert_eq!(error.body.message, CONTENT_FLAGGED);
        assert!(error.upstream_detail().is_some());
        // A named code is a bad request, not a busy server to try again.
        let named = serde_json::json!({"error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model."}});
        let error = stream_error(&named);
        assert_eq!(error.status, http::StatusCode::BAD_REQUEST);
        assert!(!error.retryable);
        assert!(error.body.message.contains("maximum context length"));
        // OpenAI Responses: a failed response's error, never its instructions.
        let failed = serde_json::json!({"id":"resp_1","status":"failed","error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model."}});
        let error = stream_request_refusal(&failed).expect("refused");
        assert_eq!(error.status, http::StatusCode::BAD_REQUEST);
        let busy = serde_json::json!({"id":"resp_2","status":"failed","instructions":"Say when the maximum context length is reached; never post content_filter words.","error":{"code":"server_error","message":"The server had an error"}});
        assert!(stream_request_refusal(&busy).is_none());
        let event = serde_json::json!({"type":"error","code":"invalid_prompt","message":"Invalid prompt: your prompt was flagged as potentially violating our usage policy."});
        assert_eq!(
            stream_request_refusal(&event)
                .expect("flagged")
                .body
                .message,
            CONTENT_FLAGGED
        );
        // OpenRouter's moderation refuses with 403; a key's 403 keeps its words.
        let input_flagged = r#"{"error":{"code":403,"message":"Your chosen model requires moderation and your input was flagged"}}"#;
        assert_eq!(
            plain_refusal(http::StatusCode::FORBIDDEN, input_flagged, None)
                .unwrap()
                .1,
            CONTENT_FLAGGED
        );
        assert_eq!(
            plain_refusal(
                http::StatusCode::FORBIDDEN,
                r#"{"error":"forbidden"}"#,
                None
            )
            .unwrap()
            .1,
            "The provider refused this request for this API key. Check what the key is allowed to use."
        );
        // A rate limit named inside a stream stays one.
        let limited = serde_json::json!({"error":{"code":"rate_limit_exceeded","message":"Rate limit reached; too many tokens per minute"}});
        assert_eq!(
            stream_error(&limited).status,
            http::StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[test]
    fn an_account_out_of_credit_reads_the_same_from_every_provider() {
        use super::{OUT_OF_CREDIT, out_of_credit, stream_error};
        let refusals = [
            (
                400,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."}}"#,
            ),
            (
                429,
                r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota","param":null,"code":"insufficient_quota"}}"#,
            ),
            (
                402,
                r#"{"error":{"message":"Insufficient credits. Add more using https://openrouter.ai/settings/credits","code":402}}"#,
            ),
            (
                402,
                r#"{"error":{"message":"This request requires more credits, or fewer max_tokens. You requested up to 32768 tokens, but can only afford 811.","code":402}}"#,
            ),
            (
                402,
                r#"{"error":{"message":"Insufficient Balance","type":"unknown_error","param":null,"code":"invalid_request_error"}}"#,
            ),
            (
                403,
                r#"{"code":"The caller does not have permission to execute the specified operation","error":"Your team has either used all available credits or reached its monthly spending limit."}"#,
            ),
        ];
        for (status, body) in refusals {
            assert!(
                out_of_credit(http::StatusCode::from_u16(status).unwrap(), body),
                "{status} {body}"
            );
        }
        // A rate limit, a per-minute quota and a plain bad request are not.
        for (status, body) in [
            (
                429,
                r#"{"error":{"message":"Rate limit reached for requests","type":"requests","code":"rate_limit_exceeded"}}"#,
            ),
            (
                429,
                r#"{"error":{"code":429,"message":"You exceeded your current quota. Quota exceeded for metric: generate_content_free_tier_requests, limit: 15","status":"RESOURCE_EXHAUSTED"}}"#,
            ),
            (
                400,
                r#"{"error":{"type":"invalid_request_error","message":"max_tokens: 64001 > 64000"}}"#,
            ),
            (
                500,
                r#"{"error":{"message":"insufficient credits on the backend"}}"#,
            ),
        ] {
            assert!(
                !out_of_credit(http::StatusCode::from_u16(status).unwrap(), body),
                "{status} {body}"
            );
        }
        let event = serde_json::json!({"error":{"code":402,"message":"Insufficient credits"}});
        let error = stream_error(&event);
        assert!(error.is_out_of_credit());
        assert_eq!(error.body.message, OUT_OF_CREDIT);
    }

    #[test]
    fn an_error_inside_a_stream_keeps_its_status_and_the_providers_words() {
        use super::stream_error;
        // OpenRouter passes the serving provider's status on as a number.
        let busy = serde_json::json!({"error":{"code":429,"message":"deepseek/deepseek-chat is temporarily rate-limited upstream. Please retry shortly.","metadata":{"provider_name":"DeepSeek"}}});
        let error = stream_error(&busy);
        assert_eq!(error.status, http::StatusCode::TOO_MANY_REQUESTS);
        assert!(error.retryable);
        assert_eq!(
            error.body.message,
            "The provider is rate limiting this key. Try again shortly."
        );
        assert_eq!(
            error.upstream_detail(),
            Some(
                "deepseek/deepseek-chat is temporarily rate-limited upstream. Please retry shortly. (DeepSeek)"
            )
        );
        let refused = serde_json::json!({"error":{"code":400,"message":"Provider returned error","metadata":{"raw":"{\"message\":\"invalid tool call\"}","provider_name":"Cohere"}}});
        let error = stream_error(&refused);
        assert_eq!(error.status, http::StatusCode::BAD_REQUEST);
        assert!(!error.retryable);
        assert_eq!(error.body.message, "upstream stream reported HTTP 400");
        assert_eq!(
            error.upstream_detail(),
            Some("Provider returned error (Cohere: invalid tool call)")
        );
        let named = serde_json::json!({"error":{"code":"server_error","message":"The server had an error"}});
        let error = stream_error(&named);
        assert_eq!(error.status, http::StatusCode::BAD_GATEWAY);
        assert!(error.retryable);
        assert_eq!(error.body.message, "upstream stream reported server_error");
        let bare = serde_json::json!({"error":{"message":"something broke"}});
        assert_eq!(
            stream_error(&bare).body.message,
            "upstream stream reported upstream_stream_error"
        );
    }

    #[test]
    fn recognizes_fragment_boundaries() {
        assert_eq!(find_boundary(b"data: {}\n\nrest"), Some((8, 2)));
        assert_eq!(find_boundary(b"data: {}\r\n\r\nrest"), Some((8, 4)));
    }

    #[test]
    fn reads_each_provider_request_id_header_and_skips_unsafe_values() {
        for name in ["x-request-id", "request-id", "x-amzn-requestid"] {
            let mut headers = HeaderMap::new();
            headers.insert(name, HeaderValue::from_static("req_01ABC-def"));
            assert_eq!(
                upstream_request_id(&headers).as_deref(),
                Some("req_01ABC-def")
            );
        }
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", HeaderValue::from_static("has spaces"));
        headers.insert("request-id", HeaderValue::from_static("req_anthropic"));
        assert_eq!(
            upstream_request_id(&headers).as_deref(),
            Some("req_anthropic")
        );
        assert_eq!(upstream_request_id(&HeaderMap::new()), None);
    }
}
