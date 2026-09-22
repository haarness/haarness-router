use std::{pin::Pin, time::Duration};

use async_stream::try_stream;
use futures::{Stream, StreamExt};
use router_core::RouterError;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

const MAX_SSE_FRAME_BYTES: usize = 1024 * 1024;

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
                Err(_) => Err(RouterError::timeout("upstream stream became idle"))?,
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
    Err(RouterError::upstream(
        status,
        format!("upstream returned HTTP {}{detail}", status.as_u16()),
    ))
}

pub fn request_error(error: reqwest::Error) -> RouterError {
    let safe = if error.is_connect() {
        "failed to connect to upstream"
    } else if error.is_timeout() {
        "upstream request timed out"
    } else {
        "upstream request failed"
    };
    RouterError::transport(safe, !error.is_connect())
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
    use super::find_boundary;

    #[test]
    fn recognizes_fragment_boundaries() {
        assert_eq!(find_boundary(b"data: {}\n\nrest"), Some((8, 2)));
        assert_eq!(find_boundary(b"data: {}\r\n\r\nrest"), Some((8, 4)));
    }
}
