use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
};

use http::StatusCode;

use crate::{AppConfig, RouterError, config::split_target};

/// Alias label for a request that named `provider/model` directly.
pub const DIRECT_ROUTE: &str = "_direct";
/// Model label for a target that is not declared under `[models]`.
pub const UNDECLARED_MODEL: &str = "_undeclared";

/// Bounded labels for one routing target. Every value comes from
/// configuration, never from the model string a client sent.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RouteLabels {
    /// The configured alias the client named, or [`DIRECT_ROUTE`].
    pub alias: String,
    /// The configured provider name.
    pub provider: String,
    /// The declared model ID within the provider, or [`UNDECLARED_MODEL`].
    pub model: String,
}

impl RouteLabels {
    pub(crate) fn for_target(
        config: &AppConfig,
        alias: &str,
        target: &str,
    ) -> Result<Self, RouterError> {
        let (provider, model) = split_target(target)?;
        Ok(Self {
            alias: alias.to_owned(),
            provider: provider.to_owned(),
            model: if config.models.contains_key(target) {
                model
            } else {
                UNDECLARED_MODEL
            }
            .to_owned(),
        })
    }
}

/// Bounded class of a failed upstream call, used as a metric label.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum UpstreamFailure {
    /// The provider answered with an HTTP 4xx status.
    ClientStatus,
    /// The provider answered with an HTTP 5xx status.
    ServerStatus,
    /// Response headers, the first event, or the next chunk missed a deadline.
    Timeout,
    /// No connection to the provider could be established.
    Connect,
    /// Anything else once the request was sent: a broken, malformed or empty
    /// stream, an in-stream error event, or an unexpected status.
    Other,
}

impl UpstreamFailure {
    pub const ALL: [Self; 5] = [
        Self::ClientStatus,
        Self::ServerStatus,
        Self::Timeout,
        Self::Connect,
        Self::Other,
    ];

    #[must_use]
    pub fn from_status(status: StatusCode) -> Self {
        if status.is_client_error() {
            Self::ClientStatus
        } else if status.is_server_error() {
            Self::ServerStatus
        } else {
            Self::Other
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ClientStatus => "4xx",
            Self::ServerStatus => "5xx",
            Self::Timeout => "timeout",
            Self::Connect => "connect",
            Self::Other => "other",
        }
    }
}

/// Whether a request ID is safe to accept, log and echo: 1 to 128 characters
/// from `[A-Za-z0-9._:-]`. Applies to caller and provider IDs alike.
#[must_use]
pub fn is_safe_request_id(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

/// Upstream failure counts by route. Every series exists from startup, so
/// rates start at zero and no client-chosen model string can add one.
pub(crate) struct UpstreamFailureCounters(BTreeMap<RouteLabels, [AtomicU64; 5]>);

impl UpstreamFailureCounters {
    pub(crate) fn new(config: &AppConfig) -> Self {
        let undeclared = config.providers.keys().map(|provider| RouteLabels {
            alias: DIRECT_ROUTE.into(),
            provider: provider.clone(),
            model: UNDECLARED_MODEL.into(),
        });
        let declared = config
            .models
            .keys()
            .filter_map(|target| RouteLabels::for_target(config, DIRECT_ROUTE, target).ok());
        let aliased = config.aliases.iter().flat_map(|(alias, definition)| {
            definition
                .targets
                .iter()
                .filter_map(|target| RouteLabels::for_target(config, alias, target).ok())
        });
        Self(
            undeclared
                .chain(declared)
                .chain(aliased)
                .map(|labels| (labels, <[AtomicU64; 5]>::default()))
                .collect(),
        )
    }

    pub(crate) fn record(&self, labels: &RouteLabels, failure: UpstreamFailure) {
        if let Some(counts) = self.0.get(labels) {
            counts[failure as usize].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn snapshot(&self) -> Vec<(RouteLabels, UpstreamFailure, u64)> {
        self.0
            .iter()
            .flat_map(|(labels, counts)| {
                UpstreamFailure::ALL.into_iter().map(move |failure| {
                    (
                        labels.clone(),
                        failure,
                        counts[failure as usize].load(Ordering::Relaxed),
                    )
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DIRECT_ROUTE, RouteLabels, UNDECLARED_MODEL, UpstreamFailure, UpstreamFailureCounters,
        is_safe_request_id,
    };
    use crate::AppConfig;

    #[test]
    fn request_ids_are_bounded_to_a_log_safe_alphabet() {
        for valid in ["req_01ABC", "a", "trace:span-1.2", &"x".repeat(128)] {
            assert!(is_safe_request_id(valid), "{valid}");
        }
        for invalid in [
            "",
            &"x".repeat(129),
            "has space",
            "line\nbreak",
            "quote\"",
            "é",
        ] {
            assert!(!is_safe_request_id(invalid), "{invalid:?}");
        }
    }

    #[test]
    fn failure_series_cover_configured_routes_only() {
        let config: AppConfig = serde_json::from_value(serde_json::json!({
            "providers": {
                "first": {"kind": "openai-compatible", "base_url": "https://example.test/v1"},
                "second": {"kind": "openai-compatible", "base_url": "https://example.test/v1"}
            },
            "models": {"first/declared": {}},
            "aliases": {"fast": {"targets": ["first/declared", "second/other"]}}
        }))
        .unwrap();
        let counters = UpstreamFailureCounters::new(&config);
        let labels = |alias: &str, provider: &str, model: &str| RouteLabels {
            alias: alias.into(),
            provider: provider.into(),
            model: model.into(),
        };
        let fast_second = labels("fast", "second", UNDECLARED_MODEL);
        counters.record(&fast_second, UpstreamFailure::ServerStatus);
        counters.record(&fast_second, UpstreamFailure::ServerStatus);
        // A label set no configuration produced is never added.
        counters.record(
            &labels(DIRECT_ROUTE, "first", "client-chosen"),
            UpstreamFailure::Connect,
        );
        let snapshot = counters.snapshot();
        let mut routes = snapshot
            .iter()
            .map(|(labels, _, _)| labels.clone())
            .collect::<Vec<_>>();
        routes.dedup();
        assert_eq!(
            routes,
            [
                labels(DIRECT_ROUTE, "first", UNDECLARED_MODEL),
                labels(DIRECT_ROUTE, "first", "declared"),
                labels(DIRECT_ROUTE, "second", UNDECLARED_MODEL),
                labels("fast", "first", "declared"),
                fast_second.clone(),
            ]
        );
        assert_eq!(snapshot.len(), routes.len() * UpstreamFailure::ALL.len());
        for (labels, failure, count) in snapshot {
            let expected =
                u64::from(labels == fast_second && failure == UpstreamFailure::ServerStatus) * 2;
            assert_eq!(count, expected, "{labels:?} {}", failure.label());
        }
    }
}
