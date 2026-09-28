use std::{
    collections::{BTreeMap, HashMap},
    fmt::Write,
    sync::{Arc, Mutex},
};

use axum::{
    Json, Router as AxumRouter,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderMap, Request, StatusCode, header},
    response::Response,
    routing::post,
};
use router_core::{
    AliasConfig, AppConfig, AuthConfig, AuthStore, ModelConfig, ProviderConfig, ProviderKind,
    Router, ServerConfig, WebUiConfig,
};
use secrecy::SecretString;
use tower::ServiceExt;

#[derive(Clone)]
struct MockState {
    status: StatusCode,
    authorizations: Arc<Mutex<Vec<String>>>,
}

#[derive(Clone, Default)]
struct NativeMockState {
    credentials: Arc<Mutex<Vec<String>>>,
    bodies: Arc<Mutex<Vec<serde_json::Value>>>,
}

async fn mock_chat(State(state): State<MockState>, headers: HeaderMap) -> Response {
    state.authorizations.lock().unwrap().push(
        headers
            .get(header::AUTHORIZATION)
            .or_else(|| headers.get("x-provider-key"))
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned(),
    );
    if !state.status.is_success() {
        return Response::builder()
            .status(state.status)
            .body(Body::from("upstream failure"))
            .unwrap();
    }
    let sse = concat!(
        "data: {\"id\":\"upstream\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hello\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"upstream\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"id\":\"upstream\",\"choices\":[],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1,\"total_tokens\":3}}\n\n",
        "data: [DONE]\n\n"
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(sse))
        .unwrap()
}

async fn spawn_mock(status: StatusCode) -> (String, Arc<Mutex<Vec<String>>>) {
    let authorizations = Arc::new(Mutex::new(Vec::new()));
    let state = MockState {
        status,
        authorizations: Arc::clone(&authorizations),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            AxumRouter::new()
                .route("/chat/completions", post(mock_chat))
                .with_state(state),
        )
        .await
        .unwrap();
    });
    (format!("http://{address}"), authorizations)
}

async fn mock_anthropic(
    State(state): State<NativeMockState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    state.credentials.lock().unwrap().push(
        headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned(),
    );
    state.bodies.lock().unwrap().push(body);
    let sse = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":4}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"claude\"}}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(sse))
        .unwrap()
}

async fn mock_gemini(
    State(state): State<NativeMockState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    state.credentials.lock().unwrap().push(
        headers
            .get("x-goog-api-key")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned(),
    );
    state.bodies.lock().unwrap().push(body);
    let sse = concat!(
        "data: {\"candidates\":[{\"index\":0,\"content\":{\"parts\":[{\"text\":\"gemini\"}]},\"finishReason\":\"STOP\"}],",
        "\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":2,\"thoughtsTokenCount\":4,\"cachedContentTokenCount\":1,\"totalTokenCount\":9}}\n\n"
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(sse))
        .unwrap()
}

async fn spawn_native_mock(kind: ProviderKind) -> (String, NativeMockState) {
    let state = NativeMockState::default();
    let application = match kind.protocol() {
        router_core::ProviderProtocol::BedrockConverse => {
            panic!("Bedrock uses the event-stream fixture in router-providers")
        }
        router_core::ProviderProtocol::AnthropicMessages => AxumRouter::new()
            .route("/messages", post(mock_anthropic))
            .with_state(state.clone()),
        router_core::ProviderProtocol::GeminiGenerateContent => AxumRouter::new()
            .route("/models/{*operation}", post(mock_gemini))
            .with_state(state.clone()),
        router_core::ProviderProtocol::OpenAiChatCompletions => {
            panic!("OpenAI-compatible mocks use spawn_mock")
        }
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, application).await.unwrap();
    });
    (format!("http://{address}"), state)
}

fn provider(base_url: String) -> ProviderConfig {
    ProviderConfig {
        kind: ProviderKind::OpenaiCompatible,
        base_url: Some(base_url),
        api_key_env: None,
        auth_header: None,
        auth_scheme: None,
        capabilities: None,
        allow_unknown_fields: true,
        allow_insecure_http: true,
        connect_timeout_ms: 1_000,
        stream_idle_timeout_ms: 1_000,
        max_error_body_bytes: 4_096,
    }
}

fn native_provider(kind: ProviderKind, base_url: String) -> ProviderConfig {
    ProviderConfig {
        kind,
        base_url: Some(base_url),
        api_key_env: None,
        auth_header: None,
        auth_scheme: None,
        capabilities: None,
        allow_unknown_fields: false,
        allow_insecure_http: true,
        connect_timeout_ms: 1_000,
        stream_idle_timeout_ms: 1_000,
        max_error_body_bytes: 4_096,
    }
}

fn config(providers: BTreeMap<String, ProviderConfig>) -> AppConfig {
    AppConfig {
        server: ServerConfig {
            first_byte_timeout_ms: 1_000,
            ..ServerConfig::default()
        },
        auth: AuthConfig {
            enabled: None,
            router_key_envs: vec!["unused-in-programmatic-test".into()],
            allow_upstream_key_header: true,
        },
        web_ui: WebUiConfig::default(),
        providers,
        models: BTreeMap::new(),
        aliases: BTreeMap::new(),
    }
}

fn chat_request(model: &str, upstream_key: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header(header::AUTHORIZATION, "Bearer router-secret")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(key) = upstream_key {
        builder = builder.header("x-upstream-api-key", key);
    }
    builder
        .body(Body::from(format!(
            r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}}]}}"#
        )))
        .unwrap()
}

fn streaming_chat_request(model: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header(header::AUTHORIZATION, "Bearer router-secret")
        .header("x-upstream-api-key", "upstream-secret")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!(
            r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}}],"stream":true,"stream_options":{{"include_usage":true}}}}"#
        )))
        .unwrap()
}

fn json_chat_request(body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header(header::AUTHORIZATION, "Bearer router-secret")
        .header("x-upstream-api-key", "native-provider-secret")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn optional_auth_serves_models_and_chat_without_forwarding_client_auth() {
    let (base, seen) = spawn_mock(StatusCode::OK).await;
    let mut config = config(BTreeMap::from([("mock".into(), provider(base))]));
    config.auth = AuthConfig::default();
    config
        .models
        .insert("mock/test".into(), ModelConfig::default());
    let auth = AuthStore::from_config(&config).unwrap();
    let adapters = router_providers::build_adapters(&config).unwrap();
    let router = Router::with_provider_keys(
        config,
        adapters,
        HashMap::from([("mock".into(), SecretString::from("provider-secret"))]),
    )
    .unwrap();
    let app = router_server::app(router_server::AppState::new(router, auth));
    let models = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(models.status(), StatusCode::OK);
    let overview = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ui/api/overview")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(overview.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["security"]["router_auth_required"], false);
    for stream in [false, true] {
        let mut request = json_chat_request(
            serde_json::json!({"model":"mock/test","messages":[{"role":"user","content":"hi"}],"stream":stream}),
        );
        request.headers_mut().remove(header::AUTHORIZATION);
        request.headers_mut().remove("x-upstream-api-key");
        if stream {
            request.headers_mut().insert(
                header::AUTHORIZATION,
                "Bearer unused-client-key".parse().unwrap(),
            );
        }
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("hello"));
    }
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["Bearer provider-secret", "Bearer provider-secret"]
    );
}

#[tokio::test]
async fn keeps_router_and_upstream_credentials_separate() {
    let (base_url, authorizations) = spawn_mock(StatusCode::OK).await;
    let config = config(BTreeMap::from([("mock".into(), provider(base_url))]));
    let adapters = router_providers::build_adapters(&config).unwrap();
    let router = Router::with_provider_keys(config, adapters, HashMap::new()).unwrap();
    let app = router_server::app(router_server::AppState::new(
        router,
        AuthStore::from_keys(["router-secret"], true),
    ));

    let response = app
        .clone()
        .oneshot(chat_request("mock/test-model", Some("upstream-secret")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-haarness-provider"], "mock");
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let response: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(response["choices"][0]["message"]["content"], "hello");
    assert_eq!(response["usage"]["total_tokens"], 3);
    assert_eq!(
        authorizations.lock().unwrap().as_slice(),
        ["Bearer upstream-secret"]
    );
    let metrics = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let metrics = to_bytes(metrics.into_body(), usize::MAX).await.unwrap();
    let metrics = String::from_utf8(metrics.to_vec()).unwrap();
    assert!(metrics.contains(
        "haarness_router_route_requests_total{provider=\"mock\",model=\"_undeclared\"} 1"
    ));
    assert!(metrics.contains(
        "haarness_router_route_completed_streams_total{provider=\"mock\",model=\"_undeclared\"} 1"
    ));
}

#[tokio::test]
async fn translates_upstream_override_into_custom_provider_header() {
    let (base_url, credentials) = spawn_mock(StatusCode::OK).await;
    let mut custom = provider(base_url);
    custom.auth_header = Some("x-provider-key".into());
    let config = config(BTreeMap::from([("mock".into(), custom)]));
    let adapters = router_providers::build_adapters(&config).unwrap();
    let router = Router::with_provider_keys(config, adapters, HashMap::new()).unwrap();
    let app = router_server::app(router_server::AppState::new(
        router,
        AuthStore::from_keys(["router-secret"], true),
    ));

    let response = app
        .oneshot(chat_request("mock/test-model", Some("upstream-secret")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(credentials.lock().unwrap().as_slice(), ["upstream-secret"]);
}

#[tokio::test]
async fn streams_openai_chunks_usage_and_done_marker() {
    let (base_url, _) = spawn_mock(StatusCode::OK).await;
    let config = config(BTreeMap::from([("mock".into(), provider(base_url))]));
    let adapters = router_providers::build_adapters(&config).unwrap();
    let router = Router::with_provider_keys(config, adapters, HashMap::new()).unwrap();
    let app = router_server::app(router_server::AppState::new(
        router,
        AuthStore::from_keys(["router-secret"], true),
    ));

    let response = app
        .oneshot(streaming_chat_request("mock/test-model"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(body.contains("\"content\":\"hello\""));
    assert!(body.contains("\"total_tokens\":3"));
    assert!(body.ends_with("data: [DONE]\n\n"));
}

#[tokio::test]
async fn native_anthropic_contract_is_isolated_behind_canonical_events() {
    let (base_url, upstream) = spawn_native_mock(ProviderKind::Anthropic).await;
    let config = config(BTreeMap::from([(
        "anthropic".into(),
        native_provider(ProviderKind::Anthropic, base_url),
    )]));
    let adapters = router_providers::build_adapters(&config).unwrap();
    let router = Router::with_provider_keys(config, adapters, HashMap::new()).unwrap();
    let app = router_server::app(router_server::AppState::new(
        router,
        AuthStore::from_keys(["router-secret"], true),
    ));

    let response = app
        .oneshot(json_chat_request(serde_json::json!({
            "model": "anthropic/claude-test",
            "messages": [
                {"role":"system", "content":"instructions", "cache_control":{"type":"ephemeral"}},
                {"role":"user", "content":[{
                    "type":"text", "text":"hi", "cache_control":{"type":"ephemeral", "ttl":"1h"}
                }]}
            ],
            "max_completion_tokens": 32
        })))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-haarness-provider"], "anthropic");
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "claude");
    assert_eq!(body["usage"]["total_tokens"], 6);

    assert_eq!(
        upstream.credentials.lock().unwrap().as_slice(),
        ["native-provider-secret"]
    );
    let bodies = upstream.bodies.lock().unwrap();
    assert_eq!(bodies[0]["model"], "claude-test");
    assert_eq!(bodies[0]["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(
        bodies[0]["messages"][0]["content"][0]["cache_control"]["ttl"],
        "1h"
    );
}

#[tokio::test]
async fn native_gemini_contract_is_isolated_behind_canonical_events() {
    let (base_url, upstream) = spawn_native_mock(ProviderKind::Gemini).await;
    let config = config(BTreeMap::from([(
        "gemini".into(),
        native_provider(ProviderKind::Gemini, base_url),
    )]));
    let adapters = router_providers::build_adapters(&config).unwrap();
    let router = Router::with_provider_keys(config, adapters, HashMap::new()).unwrap();
    let app = router_server::app(router_server::AppState::new(
        router,
        AuthStore::from_keys(["router-secret"], true),
    ));

    let response = app
        .oneshot(json_chat_request(serde_json::json!({
            "model": "gemini/gemini-test",
            "messages": [{"role":"user", "content":"hi"}],
            "max_completion_tokens": 32
        })))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-haarness-provider"], "gemini");
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "gemini");
    assert_eq!(body["usage"]["total_tokens"], 9);
    assert_eq!(body["usage"]["completion_tokens"], 6);
    assert_eq!(
        body["usage"]["completion_tokens_details"]["reasoning_tokens"],
        4
    );
    assert_eq!(body["usage"]["prompt_tokens_details"]["cached_tokens"], 1);

    assert_eq!(
        upstream.credentials.lock().unwrap().as_slice(),
        ["native-provider-secret"]
    );
    let bodies = upstream.bodies.lock().unwrap();
    assert_eq!(bodies[0]["contents"][0]["parts"][0]["text"], "hi");
}

#[tokio::test]
async fn gemini_parallel_tool_chunks_keep_ids_signatures_and_history_separate() {
    use serde_json::{Value, json};

    async fn tools(State(state): State<NativeMockState>, Json(body): Json<Value>) -> Response {
        state.bodies.lock().unwrap().push(body);
        let events = [
            json!({"candidates":[{"index":0,"content":{"parts":[
                {"text":"Checking both cities."},
                {"thoughtSignature":"first-signature","functionCall":{"name":"weather","args":{"city":"Tokyo"}}}
            ]}}]}),
            json!({"candidates":[{"index":0,"content":{"parts":[
                {"thoughtSignature":"second-signature","functionCall":{"name":"weather","args":{"city":"Paris"}}}
            ]},"finishReason":"STOP"}]}),
        ];
        let sse = events.iter().fold(String::new(), |mut output, event| {
            write!(output, "data: {event}\n\n").unwrap();
            output
        });
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(sse))
            .unwrap()
    }
    let upstream = NativeMockState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = AxumRouter::new()
        .route("/models/{*operation}", post(tools))
        .with_state(upstream.clone());
    let server = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });
    let config = config(BTreeMap::from([(
        "gemini".into(),
        native_provider(ProviderKind::Gemini, format!("http://{address}")),
    )]));
    let adapters = router_providers::build_adapters(&config).unwrap();
    let router = Router::with_provider_keys(config, adapters, HashMap::new()).unwrap();
    let app = router_server::app(router_server::AppState::new(
        router,
        AuthStore::from_keys(["router-secret"], true),
    ));
    let input =
        json!({"model":"gemini/test","messages":[{"role":"user","content":"Check both cities"}]});

    let response = app
        .clone()
        .oneshot(json_chat_request(input.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let assistant = &body["choices"][0]["message"];
    let calls = assistant["tool_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 2, "separate chunks must not merge tool calls");
    assert_eq!(
        serde_json::from_str::<Value>(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap(),
        json!({"city":"Tokyo"})
    );
    assert_eq!(
        serde_json::from_str::<Value>(calls[1]["function"]["arguments"].as_str().unwrap()).unwrap(),
        json!({"city":"Paris"})
    );
    assert_ne!(calls[0]["id"], calls[1]["id"]);
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");

    let mut streamed = input.clone();
    streamed["stream"] = json!(true);
    let response = app
        .clone()
        .oneshot(json_chat_request(streamed))
        .await
        .unwrap();
    let sse = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let chunks: Vec<Value> = sse
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect();
    let streamed_calls: Vec<_> = chunks
        .iter()
        .filter_map(|chunk| chunk.pointer("/choices/0/delta/tool_calls/0"))
        .collect();
    assert_eq!(streamed_calls.len(), 2);
    for (index, call) in streamed_calls.iter().enumerate() {
        assert_eq!(call["index"], index);
        assert!(!call["id"].as_str().unwrap().is_empty());
    }
    assert_ne!(streamed_calls[0]["id"], streamed_calls[1]["id"]);
    assert_ne!(
        calls[0]["id"], streamed_calls[0]["id"],
        "generated call IDs must be unique across turns"
    );

    let mut continuation = input;
    continuation["messages"].as_array_mut().unwrap().extend([
        assistant.clone(),
        json!({"role":"tool","tool_call_id":calls[0]["id"],"content":"Tokyo: sunny"}),
        json!({"role":"tool","tool_call_id":calls[1]["id"],"content":"Paris: rainy"}),
    ]);
    let response = app.oneshot(json_chat_request(continuation)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bodies = upstream.bodies.lock().unwrap();
    let history = &bodies.last().unwrap()["contents"];
    assert_eq!(
        history[1]["parts"][1]["thoughtSignature"],
        "first-signature"
    );
    assert_eq!(
        history[1]["parts"][2]["thoughtSignature"],
        "second-signature"
    );
    assert_eq!(
        history[2]["parts"].as_array().unwrap().len(),
        2,
        "parallel results belong in one user content"
    );
    server.abort();
}

#[tokio::test]
async fn alias_falls_back_on_explicit_retryable_status() {
    let (first_url, first_auth) = spawn_mock(StatusCode::TOO_MANY_REQUESTS).await;
    let (second_url, second_auth) = spawn_mock(StatusCode::OK).await;
    let mut config = config(BTreeMap::from([
        ("first".into(), provider(first_url)),
        ("second".into(), provider(second_url)),
    ]));
    config.auth.allow_upstream_key_header = false;
    config.aliases.insert(
        "fast".into(),
        AliasConfig {
            targets: vec!["first/model".into(), "second/model".into()],
        },
    );
    config
        .models
        .insert("first/model".into(), ModelConfig::default());
    config
        .models
        .insert("second/model".into(), ModelConfig::default());
    let adapters = router_providers::build_adapters(&config).unwrap();
    let router = Router::with_provider_keys(
        config,
        adapters,
        HashMap::from([
            ("first".into(), SecretString::from("first-key".to_owned())),
            ("second".into(), SecretString::from("second-key".to_owned())),
        ]),
    )
    .unwrap();
    let app = router_server::app(router_server::AppState::new(
        router,
        AuthStore::from_keys(["router-secret"], false),
    ));

    let response = app.oneshot(chat_request("fast", None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-haarness-provider"], "second");
    assert_eq!(response.headers()["x-haarness-attempts"], "2");
    assert_eq!(first_auth.lock().unwrap().len(), 1);
    assert_eq!(second_auth.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn rejects_bad_router_key_before_contacting_upstream() {
    let (base_url, authorizations) = spawn_mock(StatusCode::OK).await;
    let config = config(BTreeMap::from([("mock".into(), provider(base_url))]));
    let adapters = router_providers::build_adapters(&config).unwrap();
    let router = Router::with_provider_keys(config, adapters, HashMap::new()).unwrap();
    let app = router_server::app(router_server::AppState::new(
        router,
        AuthStore::from_keys(["different-secret"], true),
    ));

    let response = app
        .oneshot(chat_request("mock/test-model", Some("upstream-secret")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(authorizations.lock().unwrap().is_empty());
}

#[tokio::test]
async fn invalid_requests_and_caller_owned_keys_do_not_trip_shared_model_circuits() {
    for status in [
        StatusCode::BAD_REQUEST,
        StatusCode::UNAUTHORIZED,
        StatusCode::FORBIDDEN,
        StatusCode::NOT_FOUND,
    ] {
        let (base_url, authorizations) = spawn_mock(status).await;
        let config = config(BTreeMap::from([("mock".into(), provider(base_url))]));
        let adapters = router_providers::build_adapters(&config).unwrap();
        let router = Router::with_provider_keys(config, adapters, HashMap::new()).unwrap();
        let app = router_server::app(router_server::AppState::new(
            router,
            AuthStore::from_keys(["router-secret"], true),
        ));
        for index in 0..5 {
            let response = app
                .clone()
                .oneshot(chat_request(
                    &format!("mock/new-model-{index}"),
                    Some("caller-key"),
                ))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                status,
                "4xx responses must not turn into circuit-open errors"
            );
        }
        assert_eq!(authorizations.lock().unwrap().len(), 5);
    }
}

#[tokio::test]
async fn newest_openai_models_use_responses_and_preserve_tool_continuations() {
    use serde_json::{Value, json};
    async fn responses(
        State(state): State<NativeMockState>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Response {
        assert_eq!(
            headers[header::AUTHORIZATION],
            "Bearer native-provider-secret"
        );
        state.bodies.lock().unwrap().push(body);
        let events = [
            json!({"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_test","encrypted_content":"opaque-state","summary":[]}}),
            json!({"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_test","name":"read","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","delta":"{\"path\":\"proof.txt\"}"}),
            json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_test","name":"read","arguments":"{\"path\":\"proof.txt\"}"}}),
            json!({"type":"response.completed","response":{"usage":{"input_tokens":8,"output_tokens":5,"total_tokens":13,"output_tokens_details":{"reasoning_tokens":3}}}}),
        ];
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(events.iter().fold(
                String::new(),
                |mut output, event| {
                    write!(output, "data: {event}\n\n").unwrap();
                    output
                },
            )))
            .unwrap()
    }
    let state = NativeMockState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = AxumRouter::new()
        .route("/responses", post(responses))
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });
    let config = config(BTreeMap::from([(
        "openai".into(),
        native_provider(ProviderKind::Openai, format!("http://{address}")),
    )]));
    let adapters = router_providers::build_adapters(&config).unwrap();
    let router = Router::with_provider_keys(config, adapters, HashMap::new()).unwrap();
    let app = router_server::app(router_server::AppState::new(
        router,
        AuthStore::from_keys(["router-secret"], true),
    ));
    let mut input = json!({"model":"openai/gpt-6-astra","messages":[{"role":"user","content":"read"}],"reasoning_effort":"high","tools":[{"type":"function","function":{"name":"read","parameters":{"type":"object"}}}]});
    let response = app
        .clone()
        .oneshot(json_chat_request(input.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["usage"]["total_tokens"], 13);
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    let assistant = &body["choices"][0]["message"];
    assert_eq!(assistant["tool_calls"].as_array().unwrap().len(), 1);
    assert_eq!(
        assistant["tool_calls"][0]["function"]["arguments"],
        "{\"path\":\"proof.txt\"}"
    );
    input["messages"].as_array_mut().unwrap().extend([
        assistant.clone(),
        json!({"role":"tool","tool_call_id":"call_test","content":"proof"}),
    ]);
    input["stream"] = json!(true);
    let response = app.oneshot(json_chat_request(input)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let sse = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(sse.ends_with("data: [DONE]\n\n"));
    assert!(sse.contains("call_test"));
    let bodies = state.bodies.lock().unwrap();
    assert_eq!(bodies[0]["store"], false);
    assert_eq!(bodies[0]["tools"][0]["strict"], false);
    assert_eq!(bodies[1]["input"][1]["encrypted_content"], "opaque-state");
    assert_eq!(
        bodies[1]["input"][2]["call_id"],
        bodies[1]["input"][3]["call_id"]
    );
    server.abort();
}

#[tokio::test]
async fn models_endpoint_discovers_all_pages_with_provider_credentials_and_caches_them() {
    use axum::{extract::Query, routing::get};
    use serde_json::{Value, json};
    async fn models(
        State(state): State<NativeMockState>,
        Query(query): Query<HashMap<String, String>>,
        headers: HeaderMap,
    ) -> Json<Value> {
        state
            .credentials
            .lock()
            .unwrap()
            .push(headers["x-api-key"].to_str().unwrap().to_owned());
        assert_eq!(headers["anthropic-version"], "2023-06-01");
        assert_eq!(query["limit"], "1000");
        if query.contains_key("after_id") {
            assert_eq!(query["after_id"], "claude-first");
            Json(
                json!({"data":[{"id":"claude-brand-new","display_name":"Brand New","max_input_tokens":1_000_000,"capabilities":{"effort":{"high":{"supported":true}}}}],"has_more":false}),
            )
        } else {
            Json(json!({"data":[{"id":"claude-first"}],"has_more":true,"last_id":"claude-first"}))
        }
    }
    let state = NativeMockState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = AxumRouter::new()
        .route("/models", get(models))
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });
    let config = config(BTreeMap::from([(
        "claude-proxy".into(),
        native_provider(ProviderKind::Anthropic, format!("http://{address}")),
    )]));
    let adapters = router_providers::build_adapters(&config).unwrap();
    let router = Router::with_provider_keys(
        config,
        adapters,
        HashMap::from([(
            "claude-proxy".into(),
            SecretString::from("server-provider-key"),
        )]),
    )
    .unwrap();
    let app = router_server::app(router_server::AppState::new(
        router,
        AuthStore::from_keys(["router-secret"], true),
    ));
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(state.credentials.lock().unwrap().is_empty());
    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .header(header::AUTHORIZATION, "Bearer router-secret")
                    .header("x-upstream-api-key", "must-not-be-used-for-catalog")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["data"].as_array().unwrap().len(), 2);
        let newest = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["id"] == "claude-proxy/claude-brand-new")
            .unwrap();
        assert_eq!(newest["display_name"], "Brand New");
        assert_eq!(newest["context_window"], 1_000_000);
        assert_eq!(newest["reasoning_efforts"], json!(["high"]));
    }
    assert_eq!(
        state.credentials.lock().unwrap().as_slice(),
        ["server-provider-key", "server-provider-key"]
    );
    server.abort();
}

#[tokio::test]
async fn rejects_upstream_override_on_alias_without_contacting_provider() {
    let (base_url, authorizations) = spawn_mock(StatusCode::OK).await;
    let mut config = config(BTreeMap::from([("mock".into(), provider(base_url))]));
    config.aliases.insert(
        "fast".into(),
        AliasConfig {
            targets: vec!["mock/test".into()],
        },
    );
    let adapters = router_providers::build_adapters(&config).unwrap();
    let router = Router::with_provider_keys(config, adapters, HashMap::new()).unwrap();
    let app = router_server::app(router_server::AppState::new(
        router,
        AuthStore::from_keys(["router-secret"], true),
    ));

    let response = app
        .oneshot(chat_request("fast", Some("caller-provider-key")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(authorizations.lock().unwrap().is_empty());
}

#[tokio::test]
async fn web_ui_manages_ephemeral_router_keys_without_disclosing_them() {
    let (base_url, _) = spawn_mock(StatusCode::OK).await;
    let config = config(BTreeMap::from([("mock".into(), provider(base_url))]));
    let adapters = router_providers::build_adapters(&config).unwrap();
    let router = Router::with_provider_keys(config, adapters, HashMap::new()).unwrap();
    let app = router_server::app(router_server::AppState::new(
        router,
        AuthStore::from_keys(["router-secret"], true),
    ));

    let index = app
        .clone()
        .oneshot(Request::builder().uri("/ui").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(index.status(), StatusCode::OK);
    assert!(index.headers().contains_key("content-security-policy"));
    let index = to_bytes(index.into_body(), usize::MAX).await.unwrap();
    assert!(
        String::from_utf8(index.to_vec())
            .unwrap()
            .contains("Haarness Router")
    );

    let overview = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ui/api/overview")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let overview = to_bytes(overview.into_body(), usize::MAX).await.unwrap();
    let overview = String::from_utf8(overview.to_vec()).unwrap();
    assert!(!overview.contains("api_key_env"));
    assert!(!overview.contains("router-secret"));

    let rejected = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/ui/api/keys")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"label":"SDK test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::FORBIDDEN);

    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/ui/api/keys")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-haarness-ui", "1")
                .body(Body::from(r#"{"label":"SDK test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let created = to_bytes(created.into_body(), usize::MAX).await.unwrap();
    let created: serde_json::Value = serde_json::from_slice(&created).unwrap();
    let id = created["id"].as_str().unwrap();
    let secret = created["secret"].as_str().unwrap();
    assert!(secret.starts_with("hrk_"));

    let listed = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ui/api/keys")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let listed = to_bytes(listed.into_body(), usize::MAX).await.unwrap();
    let listed_text = String::from_utf8(listed.to_vec()).unwrap();
    assert!(listed_text.contains(id));
    assert!(!listed_text.contains(secret));

    let models = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header(header::AUTHORIZATION, format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(models.status(), StatusCode::OK);

    let revoked = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/ui/api/keys/{id}"))
                .header("x-haarness-ui", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revoked.status(), StatusCode::NO_CONTENT);

    let models = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header(header::AUTHORIZATION, format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(models.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn web_ui_path_is_configurable_and_can_be_disabled() {
    let (base_url, _) = spawn_mock(StatusCode::OK).await;
    let mut custom_path_config = config(BTreeMap::from([(
        "mock".into(),
        provider(base_url.clone()),
    )]));
    custom_path_config.web_ui.path = "/control-room".into();
    let adapters = router_providers::build_adapters(&custom_path_config).unwrap();
    let router = Router::with_provider_keys(custom_path_config, adapters, HashMap::new()).unwrap();
    let custom_path_app = router_server::app(router_server::AppState::new(
        router,
        AuthStore::from_keys(["router-secret"], true),
    ));
    let response = custom_path_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/control-room")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let old_path = custom_path_app
        .oneshot(Request::builder().uri("/ui").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(old_path.status(), StatusCode::NOT_FOUND);

    let mut disabled_config = config(BTreeMap::from([("mock".into(), provider(base_url))]));
    disabled_config.web_ui.enabled = false;
    disabled_config.web_ui.basic_auth_username_env =
        Some("HAARNESS_ABSENT_UI_USERNAME_REGRESSION".into());
    disabled_config.web_ui.basic_auth_password_env =
        Some("HAARNESS_ABSENT_UI_PASSWORD_REGRESSION".into());
    let adapters = router_providers::build_adapters(&disabled_config).unwrap();
    let router = Router::with_provider_keys(disabled_config, adapters, HashMap::new()).unwrap();
    let disabled_app = router_server::app(router_server::AppState::new(
        router,
        AuthStore::from_keys(["router-secret"], true),
    ));
    let response = disabled_app
        .clone()
        .oneshot(Request::builder().uri("/ui").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let health = disabled_app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
}

#[tokio::test]
async fn vertex_uses_oauth_bearer_without_google_api_key_header() {
    async fn vertex(headers: HeaderMap) -> Response {
        assert_eq!(
            headers[header::AUTHORIZATION],
            "Bearer native-provider-secret"
        );
        assert!(headers.get("x-goog-api-key").is_none());
        Response::builder().header(header::CONTENT_TYPE,"text/event-stream")
            .body(Body::from("data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"vertex-ok\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":2,\"candidatesTokenCount\":1,\"totalTokenCount\":3}}\n\n")).unwrap()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            AxumRouter::new().route(
                "/v1/projects/test/locations/global/publishers/google/models/{model}",
                post(vertex),
            ),
        )
        .await
        .unwrap();
    });
    let mut definition = native_provider(
        ProviderKind::Gemini,
        format!("http://{addr}/v1/projects/test/locations/global/publishers/google"),
    );
    definition.auth_header = Some("authorization".into());
    definition.auth_scheme = Some("Bearer".into());
    let config = config(BTreeMap::from([("google-vertex".into(), definition)]));
    let adapters = router_providers::build_adapters(&config).unwrap();
    let router = Router::with_provider_keys(config, adapters, HashMap::new()).unwrap();
    let app = router_server::app(router_server::AppState::new(
        router,
        AuthStore::from_keys(["router-secret"], true),
    ));
    let response=app.oneshot(json_chat_request(serde_json::json!({"model":"google-vertex/gemini-test","messages":[{"role":"user","content":"hello"}],"max_tokens":10}))).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("vertex-ok"));
    server.abort();
}
