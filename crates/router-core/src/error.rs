use std::fmt;

use http::StatusCode;
use router_protocol::{ErrorBody, ErrorEnvelope};

use crate::UpstreamFailure;

#[derive(Debug, Clone)]
pub struct RouterError {
    pub status: StatusCode,
    pub body: ErrorBody,
    pub retryable: bool,
    pub ambiguous: bool,
    /// How a provider call failed, when its transport could tell. Never sent
    /// to clients; it labels the upstream failure counter.
    pub upstream_failure: Option<UpstreamFailure>,
    /// What the provider said about the failed call: its own request ID and
    /// its explanation. Logged and kept for operators, never sent; boxed so
    /// the `Err` of nearly every result stays small.
    upstream: Option<Box<Upstream>>,
    /// How many seconds the provider asked callers to wait (its
    /// `Retry-After`, at most about 18 hours), sent on as this response's own
    /// `Retry-After`. Two bytes keep the `Err` of nearly every result small.
    pub retry_after_secs: Option<std::num::NonZeroU16>,
}

#[derive(Debug, Clone, Default)]
struct Upstream {
    request_id: Option<String>,
    /// Scrubbed by [`crate::detail::scrub`].
    detail: Option<String>,
}

/// The code of a refusal because the provider account behind the key has no
/// credit left (or too little for the request).
pub const PROVIDER_OUT_OF_CREDIT: &str = "provider_out_of_credit";

impl RouterError {
    #[must_use]
    pub fn new(
        status: StatusCode,
        message: impl Into<String>,
        kind: impl Into<String>,
        code: impl Into<String>,
    ) -> Self {
        Self {
            status,
            body: ErrorBody {
                message: message.into(),
                kind: kind.into(),
                param: None,
                code: code.into(),
            },
            retryable: false,
            ambiguous: false,
            upstream_failure: None,
            upstream: None,
            retry_after_secs: None,
        }
    }

    #[must_use]
    pub fn invalid(message: impl Into<String>, param: impl Into<String>) -> Self {
        let mut error = Self::new(
            StatusCode::BAD_REQUEST,
            message,
            "invalid_request_error",
            "invalid_request",
        );
        error.body.param = Some(param.into());
        error
    }

    #[must_use]
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            message,
            "authentication_error",
            "invalid_api_key",
        )
    }

    #[must_use]
    pub fn upstream(status: StatusCode, message: impl Into<String>) -> Self {
        let retryable = status == StatusCode::REQUEST_TIMEOUT
            || status == StatusCode::TOO_MANY_REQUESTS
            || status.is_server_error();
        Self {
            status,
            body: ErrorBody {
                message: message.into(),
                kind: "upstream_error".into(),
                param: None,
                code: format!("upstream_{}", status.as_u16()),
            },
            retryable,
            ambiguous: false,
            upstream_failure: None,
            upstream: None,
            retry_after_secs: None,
        }
    }

    #[must_use]
    pub fn transport(message: impl Into<String>, ambiguous: bool) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            body: ErrorBody {
                message: message.into(),
                kind: "upstream_transport_error".into(),
                param: None,
                code: "upstream_transport_error".into(),
            },
            retryable: !ambiguous,
            ambiguous,
            upstream_failure: None,
            upstream: None,
            retry_after_secs: None,
        }
    }

    #[must_use]
    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            message,
            "timeout_error",
            "upstream_timeout",
        )
    }

    #[must_use]
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            message,
            "service_unavailable_error",
            "no_route_available",
        )
    }

    /// Marks this error as a failed provider call of the given class.
    #[must_use]
    pub fn with_upstream_failure(mut self, failure: UpstreamFailure) -> Self {
        self.upstream_failure = Some(failure);
        self
    }

    /// Keeps the provider's own explanation, scrubbed, for operators.
    #[must_use]
    pub fn with_upstream_detail(mut self, detail: Option<String>) -> Self {
        if detail.is_some() || self.upstream.is_some() {
            self.upstream.get_or_insert_default().detail = detail;
        }
        self
    }

    /// The provider's own explanation, scrubbed: for logs and operators'
    /// records, never for clients.
    #[must_use]
    pub fn upstream_detail(&self) -> Option<&str> {
        self.upstream.as_ref()?.detail.as_deref()
    }

    /// Keeps the provider's own ID for the failed call, for logs.
    #[must_use]
    pub fn with_upstream_request_id(mut self, request_id: Option<String>) -> Self {
        if request_id.is_some() || self.upstream.is_some() {
            self.upstream.get_or_insert_default().request_id = request_id;
        }
        self
    }

    /// The provider's own ID for the failed call: logged, never sent.
    #[must_use]
    pub fn upstream_request_id(&self) -> Option<&str> {
        self.upstream.as_ref()?.request_id.as_deref()
    }

    /// Whether the provider refused because the account behind the key has
    /// no credit left: waiting does not help, adding credit or another
    /// provider does.
    #[must_use]
    pub fn is_out_of_credit(&self) -> bool {
        self.body.code == PROVIDER_OUT_OF_CREDIT
    }

    /// The `Retry-After` header value for this error's response, in whole seconds.
    #[must_use]
    pub fn retry_after_header(&self) -> Option<String> {
        self.retry_after_secs.map(|wait| wait.to_string())
    }

    #[must_use]
    pub fn envelope(&self) -> ErrorEnvelope {
        ErrorEnvelope {
            error: self.body.clone(),
        }
    }
}

impl fmt::Display for RouterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.body.message)
    }
}

impl std::error::Error for RouterError {}
