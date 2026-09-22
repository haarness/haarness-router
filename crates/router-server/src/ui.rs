use axum::{
    Json, Router as AxumRouter,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::{delete, get},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use router_core::{CircuitState, RouterError, WebUiConfig};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::AppState;

const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_CSS: &str = include_str!("../assets/app.css");
const APP_JS: &str = include_str!("../assets/app.js");
const UI_MARKER_HEADER: &str = "x-haarness-ui";

#[derive(Clone)]
pub(super) struct UiState {
    pub config: WebUiConfig,
    auth: UiAuth,
}

#[derive(Clone)]
enum UiAuth {
    Disabled,
    Basic {
        username_digest: [u8; 32],
        password_digest: [u8; 32],
    },
}

impl UiState {
    pub fn new(config: WebUiConfig) -> Result<Self, RouterError> {
        if !config.enabled {
            return Ok(Self {
                config,
                auth: UiAuth::Disabled,
            });
        }
        let auth = match (
            &config.basic_auth_username_env,
            &config.basic_auth_password_env,
        ) {
            (Some(username_environment), Some(password_environment)) => {
                let username = std::env::var(username_environment).map_err(|_| {
                    RouterError::invalid(
                        "web UI Basic authentication username is unavailable",
                        "web_ui.basic_auth_username_env",
                    )
                })?;
                let password = std::env::var(password_environment).map_err(|_| {
                    RouterError::invalid(
                        "web UI Basic authentication password is unavailable",
                        "web_ui.basic_auth_password_env",
                    )
                })?;
                if username.is_empty() || password.is_empty() {
                    return Err(RouterError::invalid(
                        "web UI Basic authentication credentials cannot be empty",
                        "web_ui.basic_auth_password_env",
                    ));
                }
                UiAuth::Basic {
                    username_digest: digest(&username),
                    password_digest: digest(&password),
                }
            }
            (None, None) => UiAuth::Disabled,
            _ => {
                return Err(RouterError::invalid(
                    "web UI Basic authentication is incompletely configured",
                    "web_ui.basic_auth_username_env",
                ));
            }
        };
        Ok(Self { config, auth })
    }

    fn authorization_failure(&self, headers: &HeaderMap) -> Option<Response> {
        let UiAuth::Basic {
            username_digest,
            password_digest,
        } = &self.auth
        else {
            return None;
        };
        let authorized = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Basic "))
            .and_then(|encoded| STANDARD.decode(encoded).ok())
            .and_then(|decoded| {
                let separator = decoded.iter().position(|byte| *byte == b':')?;
                Some((
                    decoded[..separator].to_vec(),
                    decoded[separator + 1..].to_vec(),
                ))
            })
            .is_some_and(|(username, password)| {
                let username_candidate: [u8; 32] = Sha256::digest(username).into();
                let password_candidate: [u8; 32] = Sha256::digest(password).into();
                bool::from(username_digest.ct_eq(&username_candidate))
                    & bool::from(password_digest.ct_eq(&password_candidate))
            });
        if authorized {
            None
        } else {
            Some(basic_challenge())
        }
    }

    fn basic_enabled(&self) -> bool {
        matches!(self.auth, UiAuth::Basic { .. })
    }
}

pub(super) fn mount(mut router: AxumRouter<AppState>, path: &str) -> AxumRouter<AppState> {
    let slash = format!("{path}/");
    let css = format!("{path}/assets/app.css");
    let js = format!("{path}/assets/app.js");
    let overview = format!("{path}/api/overview");
    let keys = format!("{path}/api/keys");
    let key = format!("{path}/api/keys/{{id}}");
    router = router
        .route("/", get(root))
        .route(path, get(index))
        .route(&slash, get(index))
        .route(&css, get(styles))
        .route(&js, get(script))
        .route(&overview, get(overview_handler))
        .route(&keys, get(list_keys).post(create_key))
        .route(&key, delete(remove_key));
    router
}

async fn root(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = state.ui.authorization_failure(&headers) {
        return response;
    }
    Redirect::temporary(&state.ui.config.path).into_response()
}

async fn index(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = state.ui.authorization_failure(&headers) {
        return response;
    }
    let html = INDEX_HTML.replace("__HAARNESS_UI_BASE__", &state.ui.config.path);
    secured_response(StatusCode::OK, "text/html; charset=utf-8", html, true)
}

async fn styles(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = state.ui.authorization_failure(&headers) {
        return response;
    }
    secured_response(StatusCode::OK, "text/css; charset=utf-8", APP_CSS, false)
}

async fn script(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = state.ui.authorization_failure(&headers) {
        return response;
    }
    secured_response(
        StatusCode::OK,
        "text/javascript; charset=utf-8",
        APP_JS,
        false,
    )
}

async fn overview_handler(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = state.ui.authorization_failure(&headers) {
        return response;
    }
    let config = state.router.config();
    let providers = config
        .providers
        .iter()
        .map(|(name, provider)| {
            json!({
                "name": name,
                "kind": provider.kind,
                "base_url": provider.effective_base_url().unwrap_or_default(),
                "credential_configured": provider.api_key_env.is_some(),
                "capabilities": provider.effective_capabilities(),
            })
        })
        .collect::<Vec<_>>();
    let models = config
        .models
        .iter()
        .filter_map(|(id, model)| {
            let (provider_name, _) = id.split_once('/')?;
            let provider = config.providers.get(provider_name)?;
            Some(json!({
                "id": id,
                "provider": provider_name,
                "capabilities": model.capabilities.unwrap_or_else(|| provider.effective_capabilities()),
            }))
        })
        .collect::<Vec<_>>();
    let circuits = state
        .router
        .circuit_states()
        .into_iter()
        .map(|(target, circuit_state)| {
            let state = match circuit_state {
                CircuitState::Closed => "closed",
                CircuitState::Open => "open",
                CircuitState::HalfOpen => "half-open",
            };
            json!({"target": target, "state": state})
        })
        .collect::<Vec<_>>();
    json_response(json!({
        "status": "ready",
        "version": env!("CARGO_PKG_VERSION"),
        "started_at": state.created,
        "providers": providers,
        "models": models,
        "aliases": config.aliases,
        "circuits": circuits,
        "security": {
            "router_auth_required": state.auth.authentication_required(),
            "basic_auth": state.ui.basic_enabled(),
            "runtime_key_management": state.ui.config.allow_runtime_key_management,
            "upstream_key_override": state.auth.allow_upstream_key_header,
            "bootstrap_keys": state.auth.bootstrap_key_count(),
            "max_runtime_keys": state.auth.max_runtime_keys(),
            "loopback_bind": config.server.bind.ip().is_loopback(),
        },
        "endpoints": {
            "chat": "/v1/chat/completions",
            "models": "/v1/models",
            "metrics": "/metrics",
            "health": "/healthz",
            "ui": state.ui.config.path,
        }
    }))
}

async fn list_keys(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = state.ui.authorization_failure(&headers) {
        return response;
    }
    keys_response(&state)
}

#[derive(Deserialize)]
struct CreateKeyRequest {
    label: String,
}

async fn create_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateKeyRequest>,
) -> Response {
    if let Some(response) = state.ui.authorization_failure(&headers) {
        return response;
    }
    if let Some(response) = admin_write_failure(&state, &headers) {
        return response;
    }
    let label = request.label.trim();
    if label.is_empty() || label.chars().count() > 64 || label.chars().any(char::is_control) {
        return ui_error(
            StatusCode::BAD_REQUEST,
            "label must contain between 1 and 64 printable characters",
            "invalid_key_label",
        );
    }
    let id = format!("rk_{}", Uuid::now_v7().simple());
    let secret = format!("hrk_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    if let Err(error) = state
        .auth
        .add_runtime_key(id.clone(), label.to_owned(), &secret)
    {
        return router_error(error);
    }
    let mut response = (
        StatusCode::CREATED,
        Json(json!({
            "id": id,
            "label": label,
            "secret": secret,
            "notice": "This router key is shown once and is stored only as a digest by the server."
        })),
    )
        .into_response();
    secure_api_headers(response.headers_mut());
    response
}

async fn remove_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(response) = state.ui.authorization_failure(&headers) {
        return response;
    }
    if let Some(response) = admin_write_failure(&state, &headers) {
        return response;
    }
    if state.auth.remove_runtime_key(&id) {
        let mut response = StatusCode::NO_CONTENT.into_response();
        secure_api_headers(response.headers_mut());
        response
    } else {
        ui_error(
            StatusCode::NOT_FOUND,
            "runtime router key was not found",
            "key_not_found",
        )
    }
}

fn keys_response(state: &AppState) -> Response {
    let runtime_keys = state
        .auth
        .runtime_keys()
        .into_iter()
        .map(|key| {
            json!({
                "id": key.id,
                "label": key.label,
                "created_at": key.created_at,
                "last_used_at": key.last_used_at,
            })
        })
        .collect::<Vec<_>>();
    json_response(json!({
        "bootstrap_key_count": state.auth.bootstrap_key_count(),
        "router_auth_required": state.auth.authentication_required(),
        "runtime_keys": runtime_keys,
        "runtime_key_management": state.ui.config.allow_runtime_key_management,
        "max_runtime_keys": state.auth.max_runtime_keys(),
    }))
}

fn admin_write_failure(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if !state.ui.config.allow_runtime_key_management {
        return Some(ui_error(
            StatusCode::FORBIDDEN,
            "runtime router key management is disabled",
            "key_management_disabled",
        ));
    }
    if headers
        .get(UI_MARKER_HEADER)
        .and_then(|value| value.to_str().ok())
        != Some("1")
    {
        return Some(ui_error(
            StatusCode::FORBIDDEN,
            "missing same-origin UI request marker",
            "invalid_ui_request",
        ));
    }
    None
}

fn digest(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

fn basic_challenge() -> Response {
    let mut response = secured_response(
        StatusCode::UNAUTHORIZED,
        "text/plain; charset=utf-8",
        "Authentication required",
        false,
    );
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"Haarness Router\", charset=\"UTF-8\""),
    );
    response
}

fn json_response(value: Value) -> Response {
    let mut response = Json(value).into_response();
    secure_api_headers(response.headers_mut());
    response
}

fn router_error(error: RouterError) -> Response {
    let mut response = (error.status, Json(error.envelope())).into_response();
    secure_api_headers(response.headers_mut());
    response
}

fn ui_error(status: StatusCode, message: &str, code: &str) -> Response {
    let mut response = (
        status,
        Json(json!({
            "error": {"message": message, "type": "invalid_request_error", "code": code}
        })),
    )
        .into_response();
    secure_api_headers(response.headers_mut());
    response
}

fn secured_response(
    status: StatusCode,
    content_type: &'static str,
    body: impl Into<Body>,
    html: bool,
) -> Response {
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(body.into())
        .expect("static UI response is valid");
    secure_api_headers(response.headers_mut());
    if html {
        response.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data: https:; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
            ),
        );
    }
    response
}

fn secure_api_headers(headers: &mut HeaderMap) {
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, header};
    use base64::{Engine, engine::general_purpose::STANDARD};

    use super::{UiAuth, UiState, digest};

    #[test]
    fn basic_auth_checks_both_credentials() {
        let state = UiState {
            config: router_core::WebUiConfig::default(),
            auth: UiAuth::Basic {
                username_digest: digest("operator"),
                password_digest: digest("correct horse"),
            },
        };
        let mut headers = HeaderMap::new();
        let valid = STANDARD.encode("operator:correct horse");
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {valid}")).unwrap(),
        );
        assert!(state.authorization_failure(&headers).is_none());

        let invalid = STANDARD.encode("operator:wrong");
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {invalid}")).unwrap(),
        );
        assert!(state.authorization_failure(&headers).is_some());
    }
}
