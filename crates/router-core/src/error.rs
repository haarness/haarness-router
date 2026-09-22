use std::fmt;

use http::StatusCode;
use router_protocol::{ErrorBody, ErrorEnvelope};

#[derive(Debug, Clone)]
pub struct RouterError {
    pub status: StatusCode,
    pub body: ErrorBody,
    pub retryable: bool,
    pub ambiguous: bool,
}

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
