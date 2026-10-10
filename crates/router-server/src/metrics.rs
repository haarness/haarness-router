use std::{
    collections::BTreeMap,
    fmt::Write,
    sync::atomic::{AtomicU64, Ordering},
};

use router_core::{AppConfig, CircuitState, RouteLabels, UpstreamFailure};

pub struct Metrics {
    requests: AtomicU64,
    errors: AtomicU64,
    active_streams: AtomicU64,
    completed_streams: AtomicU64,
    failed_streams: AtomicU64,
    cancelled_streams: AtomicU64,
    bytes_out: AtomicU64,
    fallback_attempts: AtomicU64,
    first_event_micros: AtomicU64,
    completion_micros: AtomicU64,
    routes: BTreeMap<String, BTreeMap<String, RouteMetrics>>,
}

#[derive(Default)]
struct RouteMetrics {
    requests: AtomicU64,
    active: AtomicU64,
    completed: AtomicU64,
    failed: AtomicU64,
    cancelled: AtomicU64,
    bytes_out: AtomicU64,
    fallback_attempts: AtomicU64,
    first_event_micros: AtomicU64,
    completion_micros: AtomicU64,
}

#[derive(Clone, Copy, Debug)]
pub enum CompletionStatus {
    Completed,
    Failed,
    Cancelled,
}

impl CompletionStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Failed upstream calls, fallback attempts included, by configured route and
/// a bounded failure class.
pub fn render_upstream_failures(samples: &[(RouteLabels, UpstreamFailure, u64)]) -> String {
    let mut output = String::from("# TYPE haarness_router_upstream_failures_total counter\n");
    for (labels, failure, count) in samples {
        let _ = writeln!(
            output,
            "haarness_router_upstream_failures_total{{provider=\"{}\",model=\"{}\",alias=\"{}\",class=\"{}\"}} {count}",
            prometheus_escape(&labels.provider),
            prometheus_escape(&labels.model),
            prometheus_escape(&labels.alias),
            failure.label()
        );
    }
    output
}

pub fn render_circuit_states(states: &[(String, CircuitState)]) -> String {
    let mut output = String::from("# TYPE haarness_router_circuit_state gauge\n");
    for (target, state) in states {
        let (provider, model) = target.split_once('/').unwrap_or((target, "_invalid"));
        let value = match state {
            CircuitState::Closed => 0,
            CircuitState::Open => 1,
            CircuitState::HalfOpen => 2,
        };
        let _ = writeln!(
            output,
            "haarness_router_circuit_state{{provider=\"{}\",model=\"{}\"}} {value}",
            prometheus_escape(provider),
            prometheus_escape(model)
        );
    }
    output
}

impl Metrics {
    pub fn new(config: &AppConfig) -> Self {
        let mut routes = BTreeMap::<String, BTreeMap<String, RouteMetrics>>::new();
        for provider in config.providers.keys() {
            routes
                .entry(provider.clone())
                .or_default()
                .insert("_undeclared".into(), RouteMetrics::default());
        }
        for target in config.models.keys() {
            if let Some((provider, model)) = target.split_once('/') {
                routes
                    .entry(provider.to_owned())
                    .or_default()
                    .insert(model.to_owned(), RouteMetrics::default());
            }
        }
        Self {
            requests: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            active_streams: AtomicU64::new(0),
            completed_streams: AtomicU64::new(0),
            failed_streams: AtomicU64::new(0),
            cancelled_streams: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
            fallback_attempts: AtomicU64::new(0),
            first_event_micros: AtomicU64::new(0),
            completion_micros: AtomicU64::new(0),
            routes,
        }
    }

    pub fn request(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn start_stream(
        &self,
        provider: &str,
        model: &str,
        attempts: usize,
        first_event_micros: u64,
    ) {
        self.active_streams.fetch_add(1, Ordering::Relaxed);
        if let Some(route) = self.route(provider, model) {
            route.requests.fetch_add(1, Ordering::Relaxed);
            route.active.fetch_add(1, Ordering::Relaxed);
            route
                .fallback_attempts
                .fetch_add(attempts.saturating_sub(1) as u64, Ordering::Relaxed);
            route
                .first_event_micros
                .fetch_add(first_event_micros, Ordering::Relaxed);
        }
    }

    pub fn finish_stream(
        &self,
        provider: &str,
        model: &str,
        status: CompletionStatus,
        elapsed_micros: u64,
    ) {
        self.active_streams.fetch_sub(1, Ordering::Relaxed);
        match status {
            CompletionStatus::Completed => &self.completed_streams,
            CompletionStatus::Failed => &self.failed_streams,
            CompletionStatus::Cancelled => &self.cancelled_streams,
        }
        .fetch_add(1, Ordering::Relaxed);
        self.completion_micros
            .fetch_add(elapsed_micros, Ordering::Relaxed);
        if let Some(route) = self.route(provider, model) {
            route.active.fetch_sub(1, Ordering::Relaxed);
            match status {
                CompletionStatus::Completed => &route.completed,
                CompletionStatus::Failed => &route.failed,
                CompletionStatus::Cancelled => &route.cancelled,
            }
            .fetch_add(1, Ordering::Relaxed);
            route
                .completion_micros
                .fetch_add(elapsed_micros, Ordering::Relaxed);
        }
    }

    pub fn bytes(&self, provider: &str, model: &str, count: usize) {
        self.bytes_out.fetch_add(count as u64, Ordering::Relaxed);
        if let Some(route) = self.route(provider, model) {
            route.bytes_out.fetch_add(count as u64, Ordering::Relaxed);
        }
    }

    pub fn fallback_attempts(&self, attempts: usize) {
        self.fallback_attempts
            .fetch_add(attempts.saturating_sub(1) as u64, Ordering::Relaxed);
    }

    pub fn first_event(&self, micros: u64) {
        self.first_event_micros.fetch_add(micros, Ordering::Relaxed);
    }

    #[must_use]
    pub fn render(&self) -> String {
        let mut output = String::new();
        metric(
            &mut output,
            "haarness_router_requests_total",
            self.requests.load(Ordering::Relaxed),
        );
        metric(
            &mut output,
            "haarness_router_errors_total",
            self.errors.load(Ordering::Relaxed),
        );
        gauge(
            &mut output,
            "haarness_router_active_streams",
            self.active_streams.load(Ordering::Relaxed),
        );
        metric(
            &mut output,
            "haarness_router_completed_streams_total",
            self.completed_streams.load(Ordering::Relaxed),
        );
        metric(
            &mut output,
            "haarness_router_failed_streams_total",
            self.failed_streams.load(Ordering::Relaxed),
        );
        metric(
            &mut output,
            "haarness_router_cancelled_streams_total",
            self.cancelled_streams.load(Ordering::Relaxed),
        );
        metric(
            &mut output,
            "haarness_router_response_bytes_total",
            self.bytes_out.load(Ordering::Relaxed),
        );
        metric(
            &mut output,
            "haarness_router_fallback_attempts_total",
            self.fallback_attempts.load(Ordering::Relaxed),
        );
        metric(
            &mut output,
            "haarness_router_first_event_microseconds_total",
            self.first_event_micros.load(Ordering::Relaxed),
        );
        metric(
            &mut output,
            "haarness_router_completion_microseconds_total",
            self.completion_micros.load(Ordering::Relaxed),
        );
        for (suffix, kind) in [
            ("requests_total", "counter"),
            ("active_streams", "gauge"),
            ("completed_streams_total", "counter"),
            ("failed_streams_total", "counter"),
            ("cancelled_streams_total", "counter"),
            ("response_bytes_total", "counter"),
            ("fallback_attempts_total", "counter"),
            ("first_event_microseconds_total", "counter"),
            ("completion_microseconds_total", "counter"),
        ] {
            let _ = writeln!(output, "# TYPE haarness_router_route_{suffix} {kind}");
        }
        for (provider, models) in &self.routes {
            for (model, route) in models {
                route_sample(
                    &mut output,
                    "requests_total",
                    provider,
                    model,
                    &route.requests,
                );
                route_sample(
                    &mut output,
                    "active_streams",
                    provider,
                    model,
                    &route.active,
                );
                route_sample(
                    &mut output,
                    "completed_streams_total",
                    provider,
                    model,
                    &route.completed,
                );
                route_sample(
                    &mut output,
                    "failed_streams_total",
                    provider,
                    model,
                    &route.failed,
                );
                route_sample(
                    &mut output,
                    "cancelled_streams_total",
                    provider,
                    model,
                    &route.cancelled,
                );
                route_sample(
                    &mut output,
                    "response_bytes_total",
                    provider,
                    model,
                    &route.bytes_out,
                );
                route_sample(
                    &mut output,
                    "fallback_attempts_total",
                    provider,
                    model,
                    &route.fallback_attempts,
                );
                route_sample(
                    &mut output,
                    "first_event_microseconds_total",
                    provider,
                    model,
                    &route.first_event_micros,
                );
                route_sample(
                    &mut output,
                    "completion_microseconds_total",
                    provider,
                    model,
                    &route.completion_micros,
                );
            }
        }
        output
    }

    fn route(&self, provider: &str, model: &str) -> Option<&RouteMetrics> {
        self.routes.get(provider)?.get(model)
    }
}

fn metric(output: &mut String, name: &str, value: u64) {
    let _ = writeln!(output, "# TYPE {name} counter");
    let _ = writeln!(output, "{name} {value}");
}

fn gauge(output: &mut String, name: &str, value: u64) {
    let _ = writeln!(output, "# TYPE {name} gauge");
    let _ = writeln!(output, "{name} {value}");
}

fn route_sample(output: &mut String, suffix: &str, provider: &str, model: &str, value: &AtomicU64) {
    let name = format!("haarness_router_route_{suffix}");
    let _ = writeln!(
        output,
        "{name}{{provider=\"{}\",model=\"{}\"}} {}",
        prometheus_escape(provider),
        prometheus_escape(model),
        value.load(Ordering::Relaxed)
    );
}

fn prometheus_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}
