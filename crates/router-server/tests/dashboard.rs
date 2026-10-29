//! The embedded dashboard: what its page may load under the Content-Security-Policy, that the script finds
//! every element it looks up, and the overview fields the page reads.

use std::collections::{BTreeSet, HashMap};

use axum::{
    Router as AxumRouter,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use router_core::{AppConfig, AuthStore, Router};
use serde_json::{Value, json};
use tower::ServiceExt;

const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_CSS: &str = include_str!("../assets/app.css");
const APP_JS: &str = include_str!("../assets/app.js");

fn dashboard(web_ui: &Value) -> AxumRouter {
    let config: AppConfig = serde_json::from_value(json!({
        "auth": {"enabled": false},
        "web_ui": web_ui,
        "providers": {
            "local": {
                "kind": "openai-compatible",
                "base_url": "https://models.example.test/v1",
            }
        },
        "models": {"local/small": {}},
        "aliases": {"default": {"targets": ["local/small"]}},
    }))
    .unwrap();
    config.validate().unwrap();
    let auth = AuthStore::from_config(&config).unwrap();
    let adapters = router_providers::build_adapters(&config).unwrap();
    let router = Router::with_provider_keys(config, adapters, HashMap::new()).unwrap();
    router_server::app(router_server::AppState::new(router, auth))
}

async fn get(app: &AxumRouter, uri: &str) -> (StatusCode, header::HeaderMap, String) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

/// Every `byId("…")` argument in the script, in order of first use.
fn looked_up_ids() -> BTreeSet<&'static str> {
    APP_JS
        .split("byId(\"")
        .skip(1)
        .filter_map(|rest| rest.split_once('"').map(|(id, _)| id))
        .collect()
}

#[test]
fn page_loads_nothing_the_policy_forbids_or_from_elsewhere() {
    // script-src and style-src are 'self' only: no inline code, styles or style attributes.
    assert!(!INDEX_HTML.contains("<style"));
    assert!(!INDEX_HTML.contains(" style="));
    for script in INDEX_HTML.split("<script").skip(1) {
        assert!(
            script
                .trim_start()
                .starts_with("src=\"__HAARNESS_UI_BASE__/"),
            "inline or foreign script: {script:.80}"
        );
    }
    for attribute in ["src=\"http", "href=\"http", "src=\"//", "href=\"//"] {
        assert!(!INDEX_HTML.contains(attribute), "{attribute}");
    }
    assert!(!APP_CSS.contains("@import"));
    for url in APP_CSS.split("url(").skip(1) {
        assert!(
            url.trim_start_matches(['"', '\'']).starts_with("data:"),
            "stylesheet loads {url:.60}"
        );
    }
    // Text is set as text: no HTML parsing of router or provider output, and nothing evaluated.
    for forbidden in [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
        "eval(",
        "new Function",
        "setAttribute(\"style\"",
    ] {
        assert!(!APP_JS.contains(forbidden), "{forbidden}");
    }
    // Requests stay on this origin: the dashboard API under its path, or the router's own endpoints.
    for call in APP_JS.split("fetch(").skip(1) {
        let target = call.trim_start();
        assert!(
            target.starts_with("`${BASE}") || target.starts_with("\"/"),
            "fetch to {target:.40}"
        );
    }
}

#[test]
fn script_finds_every_element_it_looks_up() {
    let ids = looked_up_ids();
    assert!(ids.len() > 50, "found only {} lookups", ids.len());
    for id in ids {
        assert!(
            INDEX_HTML.contains(&format!("id=\"{id}\"")),
            "app.js looks up #{id}, which index.html doesn't have"
        );
    }
}

#[test]
fn dashboard_assets_stay_small() {
    let total = INDEX_HTML.len() + APP_CSS.len() + APP_JS.len();
    assert!(total < 256 * 1024, "dashboard assets grew to {total} bytes");
}

#[tokio::test]
async fn index_and_assets_are_served_under_a_custom_path_with_a_strict_policy() {
    let app = dashboard(&json!({"path": "/console"}));
    let (status, headers, body) = get(&app, "/console").await;
    assert_eq!(status, StatusCode::OK);
    let policy = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    for directive in [
        "default-src 'none'",
        "script-src 'self'",
        "style-src 'self'",
        "connect-src 'self'",
    ] {
        assert!(policy.contains(directive), "{policy}");
    }
    assert!(!policy.contains("unsafe"), "{policy}");
    assert!(body.contains("data-ui-base=\"/console\""));
    assert!(body.contains("href=\"/console/assets/app.css\""));
    assert!(body.contains("src=\"/console/assets/app.js\""));
    assert!(!body.contains("__HAARNESS_UI_BASE__"));

    for (path, kind) in [
        ("/console/assets/app.css", "text/css"),
        ("/console/assets/app.js", "text/javascript"),
    ] {
        let (status, headers, _) = get(&app, path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with(kind)
        );
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    }
}

#[tokio::test]
async fn overview_carries_every_field_the_page_reads() {
    let app = dashboard(&json!({}));
    let (status, _, body) = get(&app, "/ui/api/overview").await;
    assert_eq!(status, StatusCode::OK);
    let overview: Value = serde_json::from_str(&body).unwrap();
    for pointer in [
        "/version",
        "/started_at",
        "/providers/0/name",
        "/providers/0/kind",
        "/providers/0/base_url",
        "/providers/0/credential_configured",
        "/providers/0/capabilities/streaming",
        "/models/0/id",
        "/models/0/provider",
        "/models/0/capabilities",
        "/aliases/default/targets/0",
        "/circuits",
        "/security/router_auth_required",
        "/security/basic_auth",
        "/security/runtime_key_management",
        "/security/upstream_key_override",
        "/security/bootstrap_keys",
        "/security/max_runtime_keys",
        "/security/loopback_bind",
        "/endpoints/chat",
        "/endpoints/models",
        "/endpoints/metrics",
        "/endpoints/health",
        "/endpoints/ui",
    ] {
        assert!(
            overview.pointer(pointer).is_some(),
            "overview lacks {pointer}"
        );
    }
    assert_eq!(overview["providers"][0]["credential_configured"], false);
    assert_eq!(overview["security"]["router_auth_required"], false);

    let (status, _, body) = get(&app, "/ui/api/keys").await;
    assert_eq!(status, StatusCode::OK);
    let keys: Value = serde_json::from_str(&body).unwrap();
    for field in [
        "bootstrap_key_count",
        "router_auth_required",
        "runtime_keys",
        "runtime_key_management",
        "max_runtime_keys",
    ] {
        assert!(keys.get(field).is_some(), "keys lacks {field}");
    }
}
