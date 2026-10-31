mod metrics;
mod ui;

use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_stream::stream;
use axum::{
    Router as AxumRouter,
    body::{Body, Bytes, to_bytes},
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures::StreamExt;
use router_core::{
    AuthStore, ConfigSource, ProviderEvent, RouteLabels, RoutedStream, Router, RouterError,
    STANDARD_PROVIDERS, StandardProvider, is_safe_request_id,
};
use router_protocol::{
    AssistantMessage, ChatCompletionRequest, ChatCompletionResponse, Choice, FunctionCall,
    ModelList, ToolCall,
};
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tower_http::{
    catch_panic::CatchPanicLayer,
    trace::{DefaultMakeSpan, DefaultOnFailure, DefaultOnResponse, TraceLayer},
};
use tracing::{Level, debug, info};
use uuid::Uuid;

use metrics::{CompletionStatus, Metrics, render_circuit_states, render_upstream_failures};
use ui::UiState;

#[derive(Clone)]
pub struct AppState {
    router: Arc<Router>,
    auth: Arc<AuthStore>,
    metrics: Arc<Metrics>,
    concurrency: Arc<Semaphore>,
    max_request_bytes: usize,
    created: u64,
    ui: UiState,
    config_source: Option<ConfigSource>,
}

impl AppState {
    #[must_use]
    pub fn new(router: Router, auth: AuthStore) -> Self {
        Self::try_new(router, auth).expect("router configuration was validated")
    }

    pub fn try_new(router: Router, auth: AuthStore) -> Result<Self, RouterError> {
        let max_concurrent_requests = router.config().server.max_concurrent_requests;
        let max_request_bytes = router.config().server.max_request_bytes;
        let metrics = Arc::new(Metrics::new(router.config()));
        let ui = UiState::new(router.config().web_ui.clone())?;
        Ok(Self {
            router: Arc::new(router),
            auth: Arc::new(auth),
            metrics,
            concurrency: Arc::new(Semaphore::new(max_concurrent_requests)),
            max_request_bytes,
            created: unix_timestamp(),
            ui,
            config_source: None,
        })
    }

    /// Records where the configuration came from, for the dashboard and for
    /// requests that name a provider the router has not set up.
    #[must_use]
    pub fn with_config_source(mut self, source: ConfigSource) -> Self {
        self.config_source = Some(source);
        self
    }
}

pub fn app(state: AppState) -> AxumRouter {
    let ui_enabled = state.ui.config.enabled;
    let ui_path = state.ui.config.path.clone();
    let mut application = AxumRouter::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/models", get(models))
        .route("/healthz", get(health))
        .route("/readyz", get(health))
        .route("/metrics", get(metrics));
    if ui_enabled {
        application = ui::mount(application, &ui_path);
    }
    application
        .with_state(state)
        .layer(CatchPanicLayer::new())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(
                    DefaultMakeSpan::new()
                        .level(Level::DEBUG)
                        .include_headers(false),
                )
                .on_response(DefaultOnResponse::new().level(Level::DEBUG))
                .on_failure(DefaultOnFailure::new().level(Level::DEBUG)),
        )
}

async fn chat_completions(State(state): State<AppState>, request: Request<Body>) -> Response {
    state.metrics.request();
    let mut log = RequestLog::new(request.headers());
    match handle_chat(&state, request, &mut log).await {
        Ok(response) => response,
        Err(error) => {
            state.metrics.error();
            let outcome = if log.attempts == 0 {
                "rejected"
            } else {
                "failed"
            };
            log.emit(Some(error.status), outcome, Some(&error.body.code));
            api_error(error, &log.request_id)
        }
    }
}

/// Routes one chat request. Once a route is chosen the request guard owns
/// the summary log line; an `Err` means the caller still has to write it.
async fn handle_chat(
    state: &AppState,
    request: Request<Body>,
    log: &mut RequestLog,
) -> Result<Response, RouterError> {
    authenticate(state, request.headers())?;
    let permit = state.concurrency.clone().try_acquire_owned().map_err(|_| {
        RouterError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "router concurrency limit reached",
            "rate_limit_error",
            "router_overloaded",
        )
    })?;
    let upstream_override = upstream_override(state, request.headers())?;
    let body = to_bytes(request.into_body(), state.max_request_bytes)
        .await
        .map_err(|_| {
            RouterError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds the configured limit",
                "invalid_request_error",
                "request_too_large",
            )
        })?;
    let chat_request: ChatCompletionRequest = serde_json::from_slice(&body)
        .map_err(|error| RouterError::invalid(format!("invalid JSON request: {error}"), "body"))?;
    if let Some(error) = setup_error(state, &chat_request.model) {
        return Err(error);
    }
    let wants_stream = chat_request.stream;
    let include_usage = chat_request.wants_usage_stream();
    let cancel = CancellationToken::new();
    let routed = match state
        .router
        .route_with_context(chat_request, upstream_override, cancel.clone())
        .await
    {
        Ok(routed) => routed,
        Err(failure) => {
            log.labels = failure.labels;
            log.attempts = failure.attempts;
            log.upstream_request_id = failure
                .error
                .upstream_request_id
                .as_deref()
                .map(str::to_owned);
            return Err(failure.error);
        }
    };
    let labels = routed.labels();
    log.labels = Some(labels.clone());
    log.attempts = routed.attempts;
    log.upstream_request_id
        .clone_from(&routed.upstream_request_id);
    let first_event_micros = u64::try_from(log.started.elapsed().as_micros()).unwrap_or(u64::MAX);
    state.metrics.fallback_attempts(routed.attempts);
    state.metrics.first_event(first_event_micros);
    state.metrics.start_stream(
        &labels.provider,
        &labels.model,
        routed.attempts,
        first_event_micros,
    );
    // Configured labels only: the raw model string stays out of logs.
    debug!(
        request_id = %log.request_id,
        alias = %labels.alias,
        provider = %labels.provider,
        model = %labels.model,
        attempts = routed.attempts,
        stream = wants_stream,
        "request routed"
    );
    if wants_stream {
        // The 200 and its headers go out before the first frame.
        let guard = RequestGuard::new(
            cancel,
            permit,
            Arc::clone(&state.metrics),
            labels,
            log.clone(),
            Some(StatusCode::OK),
        );
        Ok(streaming_response(
            routed,
            include_usage,
            guard,
            Arc::clone(&state.metrics),
        ))
    } else {
        let mut guard = RequestGuard::new(
            cancel,
            permit,
            Arc::clone(&state.metrics),
            labels,
            log.clone(),
            None,
        );
        match nonstream_response(routed, &log.request_id).await {
            Ok(response) => {
                guard.http_status = Some(StatusCode::OK);
                guard.finish(CompletionStatus::Completed, None);
                Ok(response)
            }
            Err(error) => {
                state.metrics.error();
                guard.http_status = Some(error.status);
                guard.finish(CompletionStatus::Failed, Some(&error.body.code));
                Ok(api_error(error, &log.request_id))
            }
        }
    }
}

/// A request the router cannot serve until a provider is set up, answered
/// with what to set instead of a bare unknown-provider error.
fn setup_error(state: &AppState, model: &str) -> Option<RouterError> {
    let config = state.router.config();
    if config.aliases.contains_key(model) {
        return None;
    }
    if matches!(state.config_source, Some(ConfigSource::Environment))
        && let Some(standard) = model
            .split_once('/')
            .map(|(provider, _)| provider)
            .filter(|provider| !config.providers.contains_key(*provider))
            .and_then(StandardProvider::named)
    {
        return Some(not_configured(
            format!(
                "Provider {} is not set up on this router: set {} where the router runs, then restart it.",
                standard.name,
                standard.key_variables.join(" or ")
            ),
            "provider_not_configured",
        ));
    }
    if !config.providers.is_empty() {
        return None;
    }
    let message = match &state.config_source {
        Some(ConfigSource::Environment) => format!(
            "This router has no model providers yet: set a provider API key where the router runs, then restart it. It looks for {}.",
            STANDARD_PROVIDERS
                .iter()
                .flat_map(|provider| provider.key_variables.iter().copied())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Some(ConfigSource::File(path)) => format!(
            "This router has no model providers: add one under [providers] in {}, then restart it.",
            path.display()
        ),
        Some(ConfigSource::Stdin) | None => {
            "This router has no model providers: add one under [providers] in its configuration, then restart it.".to_owned()
        }
    };
    Some(not_configured(message, "no_providers_configured"))
}

fn not_configured(message: String, code: &str) -> RouterError {
    let mut error = RouterError::invalid(message, "model");
    code.clone_into(&mut error.body.code);
    error
}

async fn models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(error) = authenticate(&state, &headers) {
        return api_error(error, &request_id(&headers));
    }
    state.router.refresh_models().await;
    axum::Json(ModelList {
        object: "list".into(),
        data: state.router.models(state.created),
    })
    .into_response()
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, axum::Json(json!({"status": "ok"})))
}

async fn metrics(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(error) = authenticate_metrics(&state, &headers) {
        return api_error(error, &request_id(&headers));
    }
    let mut output = state.metrics.render();
    output.push_str(&render_upstream_failures(&state.router.upstream_failures()));
    output.push_str(&render_circuit_states(&state.router.circuit_states()));
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        output,
    )
        .into_response()
}

fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<(), RouterError> {
    if !state.auth.authentication_required() {
        return Ok(());
    }
    if !state.auth.authenticate(bearer_token(headers)?) {
        return Err(RouterError::unauthorized("invalid router API key"));
    }
    Ok(())
}

/// `/metrics` is open exactly when the model API is. With authentication on
/// it takes a router key or the scrape token, which cannot call models.
fn authenticate_metrics(state: &AppState, headers: &HeaderMap) -> Result<(), RouterError> {
    if !state.auth.authentication_required() {
        return Ok(());
    }
    if !state.auth.authenticate_metrics(bearer_token(headers)?) {
        return Err(RouterError::unauthorized(
            "invalid router API key or metrics token",
        ));
    }
    Ok(())
}

fn bearer_token(headers: &HeaderMap) -> Result<&str, RouterError> {
    let header = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| RouterError::unauthorized("missing Authorization bearer token"))?;
    header
        .strip_prefix("Bearer ")
        .ok_or_else(|| RouterError::unauthorized("Authorization must use Bearer authentication"))
}

fn upstream_override(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<SecretString>, RouterError> {
    let name = HeaderName::from_static("x-upstream-api-key");
    let Some(value) = headers.get(name) else {
        return Ok(None);
    };
    if !state.auth.allow_upstream_key_header {
        return Err(RouterError::invalid(
            "X-Upstream-API-Key is disabled",
            "X-Upstream-API-Key",
        ));
    }
    let value = value.to_str().map_err(|_| {
        RouterError::invalid("invalid upstream API key header", "X-Upstream-API-Key")
    })?;
    if value.is_empty() {
        return Err(RouterError::invalid(
            "X-Upstream-API-Key cannot be empty",
            "X-Upstream-API-Key",
        ));
    }
    Ok(Some(SecretString::from(value.to_owned())))
}

fn streaming_response(
    mut routed: RoutedStream,
    include_usage: bool,
    mut guard: RequestGuard,
    metrics: Arc<Metrics>,
) -> Response {
    let response_id = format!("chatcmpl-{}", Uuid::now_v7());
    let model = format!("{}/{}", routed.provider, routed.model);
    let created = unix_timestamp();
    let request_id = guard.log.request_id.clone();
    let provider = routed.provider.clone();
    let served_model = routed.model.clone();
    let attempts = routed.attempts;
    let exclude_reasoning = routed.exclude_reasoning;
    let metric_provider = guard.labels.provider.clone();
    let metric_model = guard.labels.model.clone();
    let output = stream! {
        let mut announced = BTreeSet::new();
        let mut failure = None;
        while let Some(item) = routed.stream.next().await {
            let payload = match item {
                Ok(ProviderEvent::Usage(usage)) if !include_usage => {
                    let _ = usage;
                    continue;
                }
                // Internal: only the final usage reaches clients.
                Ok(ProviderEvent::PromptUsage(_)) => continue,
                Ok(ProviderEvent::ReasoningDelta { .. }) if exclude_reasoning => continue,
                // Without reasoning text the details still travel, for the next turn's replay.
                Ok(ProviderEvent::Reasoning { index, details, .. }) if exclude_reasoning => chunk_for_event(
                    &response_id,
                    &model,
                    created,
                    ProviderEvent::Reasoning { index, text: String::new(), details },
                    &mut announced,
                ),
                Ok(event) => chunk_for_event(&response_id, &model, created, event, &mut announced),
                Err(error) => {
                    metrics.error();
                    failure = Some(error.body.code.clone());
                    serde_json::to_value(error.envelope()).unwrap_or_else(|_| json!({
                        "error": {"message":"stream failed","type":"upstream_error","code":"stream_error"}
                    }))
                },
            };
            let frame = format!("data: {payload}\n\n");
            metrics.bytes(&metric_provider, &metric_model, frame.len());
            yield Ok::<Bytes, Infallible>(Bytes::from(frame));
            if payload.get("error").is_some() {
                break;
            }
        }
        let done = Bytes::from_static(b"data: [DONE]\n\n");
        metrics.bytes(&metric_provider, &metric_model, done.len());
        let status = if failure.is_some() {
            CompletionStatus::Failed
        } else {
            CompletionStatus::Completed
        };
        guard.finish(status, failure.as_deref());
        yield Ok::<Bytes, Infallible>(done);
    };
    let mut response = Response::new(Body::from_stream(output));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    add_route_headers(
        &mut response,
        &provider,
        &served_model,
        attempts,
        &request_id,
    );
    response
}

async fn nonstream_response(
    mut routed: RoutedStream,
    request_id: &str,
) -> Result<Response, RouterError> {
    let response_id = format!("chatcmpl-{}", Uuid::now_v7());
    let model = format!("{}/{}", routed.provider, routed.model);
    let mut choices: BTreeMap<u32, ChoiceAccumulator> = BTreeMap::new();
    let mut usage = None;
    while let Some(event) = routed.stream.next().await {
        match event? {
            ProviderEvent::TextDelta { index, text } => {
                choices.entry(index).or_default().content.push_str(&text);
            }
            ProviderEvent::ReasoningDelta { index, text } if !routed.exclude_reasoning => {
                choices.entry(index).or_default().reasoning.push_str(&text);
            }
            ProviderEvent::ReasoningDelta { .. } => {}
            ProviderEvent::ReasoningDetail { index, detail } => merge_reasoning_detail(
                &mut choices.entry(index).or_default().reasoning_details,
                detail,
            ),
            ProviderEvent::Reasoning {
                index,
                text,
                details,
            } => {
                let choice = choices.entry(index).or_default();
                if !routed.exclude_reasoning {
                    choice.reasoning.push_str(&text);
                }
                for detail in details {
                    merge_reasoning_detail(&mut choice.reasoning_details, detail);
                }
            }
            ProviderEvent::ToolCallDelta {
                choice_index,
                tool_index,
                id,
                kind,
                name,
                arguments,
            } => {
                let tool = choices
                    .entry(choice_index)
                    .or_default()
                    .tools
                    .entry(tool_index)
                    .or_default();
                if let Some(id) = id {
                    tool.id = id;
                }
                tool.kind = kind;
                if let Some(name) = name {
                    tool.name = name;
                }
                if let Some(arguments) = arguments {
                    tool.arguments.push_str(&arguments);
                }
            }
            ProviderEvent::Usage(value) => usage = Some(value),
            ProviderEvent::PromptUsage(value) => {
                usage.get_or_insert(value);
            }
            ProviderEvent::Finish { index, reason } => {
                choices.entry(index).or_default().finish_reason = reason;
            }
        }
    }
    if choices.is_empty() {
        choices.insert(0, ChoiceAccumulator::default());
    }
    let choices = choices
        .into_iter()
        .map(|(index, choice)| Choice {
            index,
            message: AssistantMessage {
                role: "assistant".into(),
                content: (!choice.content.is_empty()).then_some(choice.content),
                tool_calls: choice
                    .tools
                    .into_iter()
                    .map(|(tool_index, tool)| {
                        let kind = if tool.kind.is_empty() {
                            "function".to_owned()
                        } else {
                            tool.kind
                        };
                        let (function, custom) = if kind == "custom" {
                            (
                                None,
                                Some(json!({"name": tool.name, "input": tool.arguments})),
                            )
                        } else {
                            (
                                Some(FunctionCall {
                                    name: tool.name,
                                    arguments: tool.arguments,
                                }),
                                None,
                            )
                        };
                        ToolCall {
                            id: if tool.id.is_empty() {
                                format!("call_{tool_index}")
                            } else {
                                tool.id
                            },
                            kind,
                            function,
                            custom,
                            extra: BTreeMap::new(),
                        }
                    })
                    .collect(),
                reasoning: (!choice.reasoning.is_empty()).then_some(choice.reasoning.clone()),
                reasoning_content: (!choice.reasoning.is_empty()).then_some(choice.reasoning),
                reasoning_details: choice.reasoning_details,
                ..AssistantMessage::default()
            },
            finish_reason: if choice.finish_reason.is_empty() {
                "stop".into()
            } else {
                choice.finish_reason
            },
            logprobs: None,
        })
        .collect();
    let response_body = ChatCompletionResponse {
        id: response_id,
        object: "chat.completion".into(),
        created: unix_timestamp(),
        model,
        choices,
        usage,
        system_fingerprint: None,
    };
    let mut response = axum::Json(response_body).into_response();
    add_route_headers(
        &mut response,
        &routed.provider,
        &routed.model,
        routed.attempts,
        request_id,
    );
    Ok(response)
}

#[derive(Default)]
struct ChoiceAccumulator {
    content: String,
    reasoning: String,
    reasoning_details: Vec<Value>,
    tools: BTreeMap<u32, ToolAccumulator>,
    finish_reason: String,
}

#[derive(Default)]
struct ToolAccumulator {
    id: String,
    kind: String,
    name: String,
    arguments: String,
}

fn merge_reasoning_detail(details: &mut Vec<Value>, incoming: Value) {
    let same_identity = |existing: &&mut Value| {
        existing.get("type") == incoming.get("type")
            && existing.get("index") == incoming.get("index")
            && existing.get("id") == incoming.get("id")
    };
    if let Some(existing) = details.iter_mut().find(same_identity) {
        if let (Some(existing), Some(incoming)) = (existing.as_object_mut(), incoming.as_object()) {
            for (key, value) in incoming {
                if matches!(key.as_str(), "text" | "summary" | "data" | "signature")
                    && value.is_string()
                    && existing.get(key).is_some_and(Value::is_string)
                {
                    let suffix = value.as_str().unwrap_or_default();
                    let current = existing
                        .get(key)
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    existing.insert(key.clone(), Value::String(format!("{current}{suffix}")));
                } else {
                    existing.insert(key.clone(), value.clone());
                }
            }
        }
    } else {
        details.push(incoming);
    }
}

fn chunk_for_event(
    response_id: &str,
    model: &str,
    created: u64,
    event: ProviderEvent,
    announced: &mut BTreeSet<u32>,
) -> Value {
    let base = |choices: Value, usage: Value| {
        json!({
            "id": response_id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": choices,
            "usage": usage
        })
    };
    match event {
        ProviderEvent::Usage(usage) => base(
            json!([]),
            serde_json::to_value(usage).unwrap_or(Value::Null),
        ),
        // The stream skips it; an empty chunk if it ever arrives here.
        ProviderEvent::PromptUsage(_) => base(json!([]), Value::Null),
        ProviderEvent::Finish { index, reason } => base(
            json!([{
                "index": index, "delta": {}, "finish_reason": reason, "logprobs": null
            }]),
            Value::Null,
        ),
        ProviderEvent::TextDelta { index, text } => {
            let role = announced.insert(index).then_some("assistant");
            base(
                json!([{"index":index,"delta":{"role":role,"content":text},"finish_reason":null,"logprobs":null}]),
                Value::Null,
            )
        }
        ProviderEvent::ReasoningDelta { index, text } => {
            let role = announced.insert(index).then_some("assistant");
            base(
                json!([{"index":index,"delta":{"role":role,"reasoning":text,"reasoning_content":text},"finish_reason":null,"logprobs":null}]),
                Value::Null,
            )
        }
        ProviderEvent::ReasoningDetail { index, detail } => {
            let role = announced.insert(index).then_some("assistant");
            base(
                json!([{"index":index,"delta":{"role":role,"reasoning_details":[detail]},"finish_reason":null,"logprobs":null}]),
                Value::Null,
            )
        }
        ProviderEvent::Reasoning {
            index,
            text,
            details,
        } => {
            let role = announced.insert(index).then_some("assistant");
            let delta = if text.is_empty() {
                json!({"role":role,"reasoning_details":details})
            } else {
                json!({"role":role,"reasoning":text,"reasoning_content":text,"reasoning_details":details})
            };
            base(
                json!([{"index":index,"delta":delta,"finish_reason":null,"logprobs":null}]),
                Value::Null,
            )
        }
        ProviderEvent::ToolCallDelta {
            choice_index,
            tool_index,
            id,
            kind,
            name,
            arguments,
        } => {
            let role = announced.insert(choice_index).then_some("assistant");
            let call = if kind == "custom" {
                json!({"index":tool_index,"id":id,"type":kind,"custom":{"name":name,"input":arguments}})
            } else {
                json!({"index":tool_index,"id":id,"type":kind,"function":{"name":name,"arguments":arguments}})
            };
            base(
                json!([{"index":choice_index,"delta":{"role":role,"tool_calls":[call]},"finish_reason":null,"logprobs":null}]),
                Value::Null,
            )
        }
    }
}

fn add_route_headers(
    response: &mut Response,
    provider: &str,
    model: &str,
    attempts: usize,
    request_id: &str,
) {
    let headers = response.headers_mut();
    insert_header(headers, "x-request-id", request_id);
    insert_header(headers, "x-haarness-provider", provider);
    insert_header(headers, "x-haarness-model", model);
    insert_header(headers, "x-haarness-attempts", &attempts.to_string());
}

fn insert_header(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(HeaderName::from_static(name), value);
    }
}

fn api_error(error: RouterError, request_id: &str) -> Response {
    let status = error.status;
    let mut response = (status, axum::Json(error.envelope())).into_response();
    insert_header(response.headers_mut(), "x-request-id", request_id);
    response
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// One request's INFO summary line. Every field is a router ID, a validated
/// caller or provider ID, or a configuration label: never a prompt, header
/// value, credential, or the model string the client sent.
#[derive(Clone)]
struct RequestLog {
    request_id: String,
    trace_id: Option<String>,
    started: Instant,
    labels: Option<RouteLabels>,
    attempts: usize,
    upstream_request_id: Option<String>,
}

impl RequestLog {
    fn new(headers: &HeaderMap) -> Self {
        Self {
            request_id: request_id(headers),
            trace_id: trace_id(headers),
            started: Instant::now(),
            labels: None,
            attempts: 0,
            upstream_request_id: None,
        }
    }

    fn emit(&self, status: Option<StatusCode>, outcome: &str, error_code: Option<&str>) {
        let labels = self.labels.as_ref();
        info!(
            request_id = %self.request_id,
            trace_id = self.trace_id.as_deref(),
            alias = labels.map(|labels| labels.alias.as_str()),
            provider = labels.map(|labels| labels.provider.as_str()),
            model = labels.map(|labels| labels.model.as_str()),
            status = status.map(|status| status.as_u16()),
            outcome,
            error_code,
            fallbacks = self.attempts.saturating_sub(1),
            upstream_request_id = self.upstream_request_id.as_deref(),
            duration_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "request finished"
        );
    }
}

/// The caller's `X-Request-ID` when it is safe to log and echo, otherwise a
/// fresh router ID.
fn request_id(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| is_safe_request_id(value))
        .map_or_else(|| format!("req_{}", Uuid::now_v7()), str::to_owned)
}

/// The trace ID of the caller's W3C `traceparent`, so logs join its trace.
/// Malformed or repeated headers are ignored; none is forwarded upstream.
fn trace_id(headers: &HeaderMap) -> Option<String> {
    let mut values = headers.get_all("traceparent").iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    parse_traceparent(value).map(str::to_owned)
}

/// `version-traceid-parentid-flags` in lowercase hex. Version `ff` and
/// all-zero IDs are invalid; version `00` has exactly these four fields,
/// while a later version may append more after another dash.
fn parse_traceparent(value: &str) -> Option<&str> {
    let bytes = value.as_bytes();
    if bytes.len() < 55 || bytes[2] != b'-' || bytes[35] != b'-' || bytes[52] != b'-' {
        return None;
    }
    let (version, trace, parent, flags) =
        (&bytes[..2], &bytes[3..35], &bytes[36..52], &bytes[53..55]);
    let lower_hex = |field: &[u8]| {
        field
            .iter()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    };
    let nonzero = |field: &[u8]| field.iter().any(|byte| *byte != b'0');
    let length_valid = if version == b"00" {
        bytes.len() == 55
    } else {
        bytes.len() == 55 || bytes[55] == b'-'
    };
    (length_valid
        && version != b"ff"
        && [version, trace, parent, flags].into_iter().all(lower_hex)
        && nonzero(trace)
        && nonzero(parent))
    .then(|| &value[3..35])
}

struct RequestGuard {
    cancel: CancellationToken,
    _permit: OwnedSemaphorePermit,
    metrics: Arc<Metrics>,
    started: Instant,
    labels: RouteLabels,
    log: RequestLog,
    /// The status the client received, once it is known.
    http_status: Option<StatusCode>,
    finished: bool,
}

impl RequestGuard {
    fn new(
        cancel: CancellationToken,
        permit: OwnedSemaphorePermit,
        metrics: Arc<Metrics>,
        labels: RouteLabels,
        log: RequestLog,
        http_status: Option<StatusCode>,
    ) -> Self {
        Self {
            cancel,
            _permit: permit,
            metrics,
            started: Instant::now(),
            labels,
            log,
            http_status,
            finished: false,
        }
    }

    fn finish(&mut self, status: CompletionStatus, error_code: Option<&str>) {
        if self.finished {
            return;
        }
        self.finished = true;
        let elapsed = u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.metrics
            .finish_stream(&self.labels.provider, &self.labels.model, status, elapsed);
        self.log.emit(self.http_status, status.label(), error_code);
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        if !self.finished {
            self.finish(CompletionStatus::Cancelled, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::{parse_traceparent, request_id, trace_id};

    const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";

    #[test]
    fn accepts_safe_caller_request_ids_and_replaces_others() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-request-id",
            HeaderValue::from_static("client-42:retry.1"),
        );
        assert_eq!(request_id(&headers), "client-42:retry.1");
        for unsafe_id in ["has space", "semi;colon", &"x".repeat(129)] {
            headers.insert("x-request-id", HeaderValue::from_str(unsafe_id).unwrap());
            let replaced = request_id(&headers);
            assert!(replaced.starts_with("req_"), "{unsafe_id}: {replaced}");
        }
        assert!(request_id(&HeaderMap::new()).starts_with("req_"));
    }

    #[test]
    fn parses_only_strict_w3c_traceparent_values() {
        let valid = format!("00-{TRACE}-00f067aa0ba902b7-01");
        assert_eq!(parse_traceparent(&valid), Some(TRACE));
        // A later version may carry more fields after another dash.
        let future = format!("01-{TRACE}-00f067aa0ba902b7-09-extra");
        assert_eq!(parse_traceparent(&future), Some(TRACE));
        for invalid in [
            format!("00-{TRACE}-00f067aa0ba902b7-01-extra"),
            format!("ff-{TRACE}-00f067aa0ba902b7-01"),
            format!("00-{}-00f067aa0ba902b7-01", TRACE.to_uppercase()),
            format!("00-{}-00f067aa0ba902b7-01", "0".repeat(32)),
            format!("00-{TRACE}-0000000000000000-01"),
            format!("00-{TRACE}-00f067aa0ba902b7-1"),
            format!("01-{TRACE}-00f067aa0ba902b7-01x"),
            format!("00_{TRACE}_00f067aa0ba902b7_01"),
            format!("00-{TRACE}-00f067aa0ba902b-701"),
            String::new(),
        ] {
            assert_eq!(parse_traceparent(&invalid), None, "{invalid}");
        }
    }

    #[test]
    fn ignores_repeated_traceparent_headers() {
        let value = HeaderValue::from_str(&format!("00-{TRACE}-00f067aa0ba902b7-01")).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("traceparent", value.clone());
        assert_eq!(trace_id(&headers).as_deref(), Some(TRACE));
        headers.append("traceparent", value);
        assert_eq!(trace_id(&headers), None);
    }
}
