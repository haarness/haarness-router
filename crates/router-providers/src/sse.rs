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
    let detail = upstream_code.map_or_else(String::new, |code| format!(" ({code})"));
    let mut error = RouterError::upstream(
        status,
        format!("upstream returned HTTP {}{detail}", status.as_u16()),
    )
    .with_upstream_failure(UpstreamFailure::from_status(status));
    error.upstream_request_id = upstream_request_id.map(String::into_boxed_str);
    Err(error)
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

    use super::{find_boundary, upstream_request_id};

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
