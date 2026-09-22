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
use router_core::{AuthStore, ProviderEvent, RoutedStream, Router, RouterError};
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
use tracing::{Level, debug};
use uuid::Uuid;

use metrics::{CompletionStatus, Metrics, render_circuit_states};
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
        })
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
    let started = Instant::now();
    let request_id = format!("req_{}", Uuid::now_v7());
    let result = handle_chat(&state, request, &request_id, started).await;
    match result {
        Ok(response) => response,
        Err(error) => {
            state.metrics.error();
            debug!(
                request_id,
                status = error.status.as_u16(),
                error_type = %error.body.kind,
                error_code = %error.body.code,
                "request rejected"
            );
            api_error(error, &request_id)
        }
    }
}

async fn handle_chat(
    state: &AppState,
    request: Request<Body>,
    request_id: &str,
    started: Instant,
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
    let wants_stream = chat_request.stream;
    let include_usage = chat_request.wants_usage_stream();
    let cancel = CancellationToken::new();
    let routed = state
        .router
        .route(chat_request, upstream_override, cancel.clone())
        .await?;
    let first_event_micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    state.metrics.fallback_attempts(routed.attempts);
    state.metrics.first_event(first_event_micros);
    state.metrics.start_stream(
        &routed.provider,
        &routed.metric_model,
        routed.attempts,
        first_event_micros,
    );
    debug!(
        request_id,
        provider = %routed.provider,
        model = %routed.model,
        attempts = routed.attempts,
        stream = wants_stream,
        "request routed"
    );
    if wants_stream {
        Ok(streaming_response(
            routed,
            request_id,
            include_usage,
            cancel,
            permit,
            Arc::clone(&state.metrics),
        ))
    } else {
        let mut guard = RequestGuard::new(
            cancel,
            permit,
            Arc::clone(&state.metrics),
            request_id,
            &routed.provider,
            &routed.metric_model,
            routed.attempts,
        );
        let response = nonstream_response(routed, request_id).await;
        guard.finish(if response.is_ok() {
            CompletionStatus::Completed
        } else {
            CompletionStatus::Failed
        });
        response
    }
}

async fn models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(error) = authenticate(&state, &headers) {
        return api_error(error, &format!("req_{}", Uuid::now_v7()));
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

async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let mut output = state.metrics.render();
    output.push_str(&render_circuit_states(&state.router.circuit_states()));
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        output,
    )
}

fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<(), RouterError> {
    if !state.auth.authentication_required() {
        return Ok(());
    }
    let header = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| RouterError::unauthorized("missing Authorization bearer token"))?;
    let candidate = header
        .strip_prefix("Bearer ")
        .ok_or_else(|| RouterError::unauthorized("Authorization must use Bearer authentication"))?;
    if !state.auth.authenticate(candidate) {
        return Err(RouterError::unauthorized("invalid router API key"));
    }
    Ok(())
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
    request_id: &str,
    include_usage: bool,
    cancel: CancellationToken,
    permit: OwnedSemaphorePermit,
    metrics: Arc<Metrics>,
) -> Response {
    let response_id = format!("chatcmpl-{}", Uuid::now_v7());
    let model = format!("{}/{}", routed.provider, routed.model);
    let created = unix_timestamp();
    let request_id_owned = request_id.to_owned();
    let metrics_for_stream = Arc::clone(&metrics);
    let provider = routed.provider.clone();
    let served_model = routed.model.clone();
    let metric_model = routed.metric_model.clone();
    let attempts = routed.attempts;
    let exclude_reasoning = routed.exclude_reasoning;
    let guard_request_id = request_id_owned.clone();
    let guard_provider = provider.clone();
    let guard_metric_model = metric_model.clone();
    let output = stream! {
        let mut guard = RequestGuard::new(
            cancel,
            permit,
            metrics_for_stream,
            &guard_request_id,
            &guard_provider,
            &guard_metric_model,
            attempts,
        );
        let mut announced = BTreeSet::new();
        let mut failed = false;
        while let Some(item) = routed.stream.next().await {
            let payload = match item {
                Ok(ProviderEvent::Usage(usage)) if !include_usage => {
                    let _ = usage;
                    continue;
                }
                Ok(ProviderEvent::ReasoningDelta { .. }) if exclude_reasoning => continue,
                Ok(event) => chunk_for_event(&response_id, &model, created, event, &mut announced),
                Err(error) => {
                    metrics.error();
                    failed = true;
                    serde_json::to_value(error.envelope()).unwrap_or_else(|_| json!({
                        "error": {"message":"stream failed","type":"upstream_error","code":"stream_error"}
                    }))
                },
            };
            let frame = format!("data: {payload}\n\n");
            metrics.bytes(&guard_provider, &guard_metric_model, frame.len());
            yield Ok::<Bytes, Infallible>(Bytes::from(frame));
            if payload.get("error").is_some() {
                break;
            }
        }
        let done = Bytes::from_static(b"data: [DONE]\n\n");
        metrics.bytes(&guard_provider, &guard_metric_model, done.len());
        guard.finish(if failed {
            CompletionStatus::Failed
        } else {
            CompletionStatus::Completed
        });
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
        &request_id_owned,
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

struct RequestGuard {
    cancel: CancellationToken,
    _permit: OwnedSemaphorePermit,
    metrics: Arc<Metrics>,
    started: Instant,
    request_id: String,
    provider: String,
    model: String,
    attempts: usize,
    finished: bool,
}

impl RequestGuard {
    fn new(
        cancel: CancellationToken,
        permit: OwnedSemaphorePermit,
        metrics: Arc<Metrics>,
        request_id: &str,
        provider: &str,
        model: &str,
        attempts: usize,
    ) -> Self {
        Self {
            cancel,
            _permit: permit,
            metrics,
            started: Instant::now(),
            request_id: request_id.to_owned(),
            provider: provider.to_owned(),
            model: model.to_owned(),
            attempts,
            finished: false,
        }
    }

    fn finish(&mut self, status: CompletionStatus) {
        if self.finished {
            return;
        }
        self.finished = true;
        let elapsed = u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.metrics
            .finish_stream(&self.provider, &self.model, status, elapsed);
        debug!(
            request_id = %self.request_id,
            provider = %self.provider,
            model = %self.model,
            attempts = self.attempts,
            status = ?status,
            completion_microseconds = elapsed,
            "request finished"
        );
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        if !self.finished {
            self.finish(CompletionStatus::Cancelled);
        }
    }
}
