use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use futures::StreamExt;
use router_core::{Capabilities, RouterError};
use router_protocol::{ChatCompletionRequest, ContentPart, MessageContent};
use url::Url;

const MAX_PDF_BYTES: usize = 20 * 1024 * 1024;
const MAX_EXTRACTED_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_REDIRECTS: usize = 3;

pub async fn prepare_pdf_fallback(
    request: &mut ChatCompletionRequest,
    capabilities: Capabilities,
) -> Result<(), RouterError> {
    if capabilities.pdf {
        return Ok(());
    }
    for message in &mut request.messages {
        let Some(MessageContent::Parts(parts)) = &mut message.content else {
            continue;
        };
        for part in parts.iter_mut().filter(|part| part.kind == "file") {
            let file = part.file.as_ref().ok_or_else(|| {
                RouterError::invalid("file part is missing file data", "messages.content.file")
            })?;
            let source = file.file_data.as_deref().ok_or_else(|| {
                RouterError::invalid(
                    "file part is missing file_data",
                    "messages.content.file.file_data",
                )
            })?;
            let bytes = if source.starts_with("https://") {
                fetch_public_https(source).await?
            } else {
                decode_pdf_data(source)?
            };
            if !is_pdf(&bytes) {
                return Err(RouterError::invalid(
                    "local file fallback accepts PDF input only",
                    "messages.content.file.file_data",
                ));
            }
            let text = pdf_extract::extract_text_from_mem(&bytes).map_err(|_| {
                RouterError::invalid(
                    "PDF text extraction failed; use a model with native PDF support",
                    "messages.content.file.file_data",
                )
            })?;
            let text = text.trim();
            if text.is_empty() {
                let mut error = RouterError::invalid(
                    "PDF contains no extractable text; scanned PDFs require native PDF support",
                    "messages.content.file.file_data",
                );
                error.body.code = "unsupported_file".into();
                return Err(error);
            }
            if text.len() > MAX_EXTRACTED_TEXT_BYTES {
                return Err(RouterError::new(
                    http::StatusCode::PAYLOAD_TOO_LARGE,
                    "extracted PDF text exceeds the 4 MiB limit",
                    "invalid_request_error",
                    "file_too_large",
                ));
            }
            let filename = file.filename.as_deref().unwrap_or("document.pdf");
            *part = ContentPart {
                kind: "text".into(),
                text: Some(format!("[Extracted PDF: {filename}]\n{text}")),
                image_url: None,
                input_audio: None,
                file: None,
                video_url: None,
                refusal: None,
                extra: BTreeMap::default(),
            };
        }
    }
    Ok(())
}

fn decode_pdf_data(source: &str) -> Result<Vec<u8>, RouterError> {
    let encoded = if let Some(payload) = source.strip_prefix("data:") {
        let (metadata, encoded) = payload.split_once(',').ok_or_else(|| {
            RouterError::invalid("malformed PDF data URL", "messages.content.file.file_data")
        })?;
        if !metadata.eq_ignore_ascii_case("application/pdf;base64") {
            return Err(RouterError::invalid(
                "file data URL must use application/pdf;base64",
                "messages.content.file.file_data",
            ));
        }
        encoded
    } else {
        source
    };
    if encoded.len() > MAX_PDF_BYTES.saturating_mul(4) / 3 + 16 {
        return Err(file_too_large());
    }
    let bytes = STANDARD.decode(encoded).map_err(|_| {
        RouterError::invalid("invalid base64 PDF data", "messages.content.file.file_data")
    })?;
    if bytes.len() > MAX_PDF_BYTES {
        return Err(file_too_large());
    }
    Ok(bytes)
}

async fn fetch_public_https(source: &str) -> Result<Vec<u8>, RouterError> {
    let mut current = Url::parse(source).map_err(|_| {
        RouterError::invalid("invalid attachment URL", "messages.content.file.file_data")
    })?;
    for redirect in 0..=MAX_REDIRECTS {
        validate_https_url(&current)?;
        let host = current.host_str().ok_or_else(|| {
            RouterError::invalid(
                "attachment URL has no host",
                "messages.content.file.file_data",
            )
        })?;
        let port = current.port_or_known_default().unwrap_or(443);
        let addresses = tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| {
                RouterError::invalid(
                    "attachment host could not be resolved",
                    "messages.content.file.file_data",
                )
            })?
            .collect::<Vec<_>>();
        if addresses.is_empty() || addresses.iter().any(|address| !is_public_ip(address.ip())) {
            return Err(RouterError::invalid(
                "attachment URL resolves to a non-public address",
                "messages.content.file.file_data",
            ));
        }
        let pinned = SocketAddr::new(addresses[0].ip(), port);
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .resolve(host, pinned)
            .build()
            .map_err(|_| RouterError::transport("failed to initialize attachment client", false))?;
        let response = client.get(current.clone()).send().await.map_err(|error| {
            RouterError::transport(
                if error.is_timeout() {
                    "attachment download timed out"
                } else {
                    "attachment download failed"
                },
                false,
            )
        })?;
        if response.status().is_redirection() {
            if redirect == MAX_REDIRECTS {
                return Err(RouterError::invalid(
                    "attachment URL redirected too many times",
                    "messages.content.file.file_data",
                ));
            }
            let location = response
                .headers()
                .get(http::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| {
                    RouterError::invalid(
                        "attachment redirect has no valid Location",
                        "messages.content.file.file_data",
                    )
                })?;
            current = current.join(location).map_err(|_| {
                RouterError::invalid(
                    "invalid attachment redirect URL",
                    "messages.content.file.file_data",
                )
            })?;
            continue;
        }
        if !response.status().is_success() {
            return Err(RouterError::upstream(
                response.status(),
                "attachment server returned an error",
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_PDF_BYTES as u64)
        {
            return Err(file_too_large());
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|_| RouterError::transport("attachment download failed", true))?;
            if bytes.len().saturating_add(chunk.len()) > MAX_PDF_BYTES {
                return Err(file_too_large());
            }
            bytes.extend_from_slice(&chunk);
        }
        return Ok(bytes);
    }
    Err(RouterError::invalid(
        "attachment download failed",
        "messages.content.file.file_data",
    ))
}

fn validate_https_url(url: &Url) -> Result<(), RouterError> {
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return Err(RouterError::invalid(
            "attachment URLs must use HTTPS and cannot contain credentials",
            "messages.content.file.file_data",
        ));
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return Err(RouterError::invalid(
            "attachment URL host is not public",
            "messages.content.file.file_data",
        ));
    }
    Ok(())
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.is_multicast())
        }
        IpAddr::V6(ip) => {
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_multicast())
        }
    }
}

fn is_pdf(bytes: &[u8]) -> bool {
    bytes.starts_with(b"%PDF-")
        || infer::get(bytes).is_some_and(|kind| kind.mime_type() == "application/pdf")
}

fn file_too_large() -> RouterError {
    RouterError::new(
        http::StatusCode::PAYLOAD_TOO_LARGE,
        "PDF exceeds the 20 MiB limit",
        "invalid_request_error",
        "file_too_large",
    )
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::is_public_ip;

    #[test]
    fn rejects_private_and_loopback_addresses() {
        assert!(!is_public_ip(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(!is_public_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(!is_public_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_public_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
    }
}
