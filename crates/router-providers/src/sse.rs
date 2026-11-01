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
        .with_upstream_failure(UpstreamFailure::from_status(status));
    error.upstream_request_id = upstream_request_id.map(String::into_boxed_str);
    // A wait helps only when the provider is busy, not with a request it will never take.
    error.retry_after_secs = retry_after
        .filter(|_| matches!(status.as_u16(), 429 | 503))
        .and_then(|wait| {
            std::num::NonZeroU16::new(u16::try_from(wait.as_secs().max(1)).unwrap_or(u16::MAX))
        });
    Err(error)
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
                Some(wait) => format!(
                    "The provider is rate limiting this key. Try again in {} seconds.",
                    wait.as_secs().max(1)
                ),
                None => "The provider is rate limiting this key. Try again shortly.".to_owned(),
            },
        )),
        _ => None,
    }
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
        assert!(plain_refusal(http::StatusCode::NOT_FOUND, "{}", None).is_none());
        assert!(plain_refusal(http::StatusCode::BAD_REQUEST, groq, None).is_none());
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
