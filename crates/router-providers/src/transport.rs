use std::time::Duration;

use router_core::{ProviderConfig, RouterError};
use tokio_util::sync::CancellationToken;

use crate::sse::{JsonSseStream, checked_response, json_sse_stream};

/// Shared, bounded HTTP transport for one configured provider instance.
///
/// Every adapter gets its own pooled client, but connection policy, error-body
/// bounds, and SSE idle handling live here so new native adapters cannot drift
/// into subtly different transport behavior.
pub(crate) struct ProviderTransport {
    client: reqwest::Client,
    idle_timeout: Duration,
    max_error_body_bytes: usize,
}

impl ProviderTransport {
    pub(crate) fn new(config: &ProviderConfig) -> Result<Self, RouterError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(config.connect_timeout_ms))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_nodelay(true)
            .build()
            .map_err(|error| RouterError::invalid(error.to_string(), "provider"))?;
        Ok(Self {
            client,
            idle_timeout: Duration::from_millis(config.stream_idle_timeout_ms),
            max_error_body_bytes: config.max_error_body_bytes,
        })
    }

    pub(crate) fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub(crate) async fn checked(
        &self,
        response: reqwest::Response,
    ) -> Result<reqwest::Response, RouterError> {
        checked_response(response, self.max_error_body_bytes).await
    }

    pub(crate) fn sse(
        &self,
        response: reqwest::Response,
        cancel: CancellationToken,
    ) -> JsonSseStream {
        json_sse_stream(response, cancel, self.idle_timeout)
    }
}
