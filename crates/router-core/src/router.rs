use std::{
    collections::{BTreeMap, HashMap},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_stream::try_stream;
use async_trait::async_trait;
use dashmap::DashMap;
use futures::{Stream, StreamExt};
use secrecy::SecretString;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio_util::sync::CancellationToken;

use router_protocol::{ChatCompletionRequest, ModelObject, Usage};

use crate::{
    AppConfig, CircuitBreaker, CircuitState, ProviderConfig, ProviderKind, Requirements,
    RouterError, config::split_target,
};

pub type EventStream = Pin<Box<dyn Stream<Item = Result<ProviderEvent, RouterError>> + Send>>;
pub type AdapterMap = HashMap<String, Arc<dyn ProviderAdapter>>;

#[derive(Clone, Debug)]
pub enum ProviderEvent {
    TextDelta {
        index: u32,
        text: String,
    },
    ReasoningDelta {
        index: u32,
        text: String,
    },
    ReasoningDetail {
        index: u32,
        detail: serde_json::Value,
    },
    ToolCallDelta {
        choice_index: u32,
        tool_index: u32,
        id: Option<String>,
        kind: String,
        name: Option<String>,
        arguments: Option<String>,
    },
    Usage(Usage),
    /// The prompt's size as the provider reports it when it starts the
    /// response (Anthropic `message_start`): input and cache counts, output
    /// so far. The final `Usage` supersedes it; it exists so a stream cut
    /// before that report is still billed from the provider's own counts.
    /// Never forwarded to clients.
    PromptUsage(Usage),
    Finish {
        index: u32,
        reason: String,
    },
}

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    /// None means discovery is unavailable; preserve the previous catalog.
    async fn discover_models(
        &self,
        _config: &ProviderConfig,
        _api_key: Option<&SecretString>,
    ) -> Option<Vec<ModelObject>> {
        None
    }

    async fn stream(
        &self,
        model: &str,
        request: ChatCompletionRequest,
        capabilities: crate::Capabilities,
        api_key: Option<&SecretString>,
        cancel: CancellationToken,
    ) -> Result<EventStream, RouterError>;
}

pub struct RoutedStream {
    pub requested_model: String,
    pub provider: String,
    pub model: String,
    /// Bounded observability label: a declared model ID or `_undeclared`.
    pub metric_model: String,
    pub attempts: usize,
    pub exclude_reasoning: bool,
    pub stream: EventStream,
}

#[derive(Clone)]
struct TargetRuntime {
    provider: String,
    model: String,
    kind: ProviderKind,
    config: ProviderConfig,
    server_key: Option<Arc<SecretString>>,
    adapter: Arc<dyn ProviderAdapter>,
    breaker: Arc<CircuitBreaker>,
}

pub struct Router {
    config: Arc<AppConfig>,
    adapters: AdapterMap,
    provider_keys: HashMap<String, Arc<SecretString>>,
    breakers: DashMap<String, Arc<CircuitBreaker>>,
    model_catalog: Mutex<HashMap<String, Vec<ModelObject>>>,
    model_refresh: tokio::sync::Mutex<Option<std::time::Instant>>,
}

impl Router {
    pub fn new(config: AppConfig, adapters: AdapterMap) -> Result<Self, RouterError> {
        let provider_keys = config
            .providers
            .iter()
            .filter_map(|(provider, definition)| {
                definition.api_key_env.as_ref().map(|environment| {
                    std::env::var(environment)
                        .map(|value| (provider.clone(), SecretString::from(value)))
                        .map_err(|_| {
                            RouterError::invalid(
                                format!(
                                    "required secret environment variable {environment} is not set"
                                ),
                                format!("providers.{provider}.api_key_env"),
                            )
                        })
                })
            })
            .collect::<Result<HashMap<_, _>, _>>()?;
        Self::with_provider_keys(config, adapters, provider_keys)
    }

    pub fn with_provider_keys(
        config: AppConfig,
        adapters: AdapterMap,
        provider_keys: HashMap<String, SecretString>,
    ) -> Result<Self, RouterError> {
        for provider in config.providers.keys() {
            if !adapters.contains_key(provider) {
                return Err(RouterError::invalid(
                    format!("no adapter was constructed for provider {provider}"),
                    format!("providers.{provider}"),
                ));
            }
        }
        let breakers = DashMap::new();
        for provider in config.providers.keys() {
            breakers.insert(
                format!("{provider}/*"),
                Arc::new(CircuitBreaker::new(3, Duration::from_secs(30))),
            );
        }
        for model in config.models.keys() {
            breakers.insert(
                model.clone(),
                Arc::new(CircuitBreaker::new(3, Duration::from_secs(30))),
            );
        }
        Ok(Self {
            model_catalog: Mutex::new(HashMap::new()),
            model_refresh: tokio::sync::Mutex::new(None),
            config: Arc::new(config),
            adapters,
            provider_keys: provider_keys
                .into_iter()
                .map(|(provider, key)| (provider, Arc::new(key)))
                .collect(),
            breakers,
        })
    }

    #[must_use]
    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    #[must_use]
    pub fn models(&self, created: u64) -> Vec<ModelObject> {
        let aliases = self.config.aliases.keys().map(|id| ModelObject {
            id: id.clone(),
            object: "model".into(),
            created,
            owned_by: "haarness-router".into(),
            metadata: BTreeMap::default(),
        });
        let concrete = self.config.models.keys().map(|id| ModelObject {
            id: id.clone(),
            object: "model".into(),
            created,
            owned_by: id
                .split_once('/')
                .map_or("unknown", |(provider, _)| provider)
                .into(),
            metadata: BTreeMap::default(),
        });
        let mut models = aliases
            .chain(concrete)
            .map(|model| (model.id.clone(), model))
            .collect::<std::collections::BTreeMap<_, _>>();
        for catalog in self
            .model_catalog
            .lock()
            .expect("model catalog lock")
            .values()
        {
            for model in catalog {
                models.insert(model.id.clone(), model.clone());
            }
        }
        models.into_values().collect()
    }

    /// Refresh account-visible models without putting discovery on the chat path.
    /// One bounded refresh serves concurrent clients; failures retain last success.
    pub async fn refresh_models(&self) {
        let mut last_refresh = self.model_refresh.lock().await;
        if last_refresh.is_some_and(|time| time.elapsed() < Duration::from_secs(300)) {
            return;
        }
        let results = futures::future::join_all(self.config.providers.iter().map(
            |(name, config)| async move {
                let adapter = self.adapters.get(name)?;
                let key = self.provider_keys.get(name).map(AsRef::as_ref);
                let mut catalog = tokio::time::timeout(
                    Duration::from_secs(10),
                    adapter.discover_models(config, key),
                )
                .await
                .ok()??;
                for model in &mut catalog {
                    model.id = format!("{name}/{}", model.id);
                    name.clone_into(&mut model.owned_by);
                }
                Some((name.clone(), catalog))
            },
        ))
        .await;
        let mut catalogs = self.model_catalog.lock().expect("model catalog lock");
        for (name, models) in results.into_iter().flatten() {
            catalogs.insert(name, models);
        }
        *last_refresh = Some(std::time::Instant::now());
    }

    #[must_use]
    pub fn circuit_states(&self) -> Vec<(String, CircuitState)> {
        let mut states = self
            .breakers
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().state()))
            .collect::<Vec<_>>();
        states.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        states
    }

    pub async fn route(
        &self,
        request: ChatCompletionRequest,
        upstream_override: Option<SecretString>,
        cancel: CancellationToken,
    ) -> Result<RoutedStream, RouterError> {
        let requirements = Requirements::from_request(&request)?;
        let exclude_reasoning = request
            .reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.exclude)
            .unwrap_or(false);
        let requested_model = request.model.clone();
        let targets = if let Some(alias) = self.config.aliases.get(&requested_model) {
            if upstream_override.is_some() {
                return Err(RouterError::invalid(
                    "X-Upstream-API-Key requires an exact provider/model and cannot be used with aliases",
                    "model",
                ));
            }
            alias.targets.clone()
        } else {
            vec![requested_model.clone()]
        };

        let mut attempts = 0;
        let mut last_retryable = None;
        let mut capability_failures = Vec::new();
        for target in targets {
            let mut runtime = self.target_runtime(&target)?;
            if upstream_override.is_some() {
                // Customer-key quotas and failures must not trip or heal the shared
                // system-credential circuit. An override has one exact target;
                // a request-local breaker avoids unbounded per-secret state.
                runtime.breaker = Arc::new(CircuitBreaker::new(3, Duration::from_secs(30)));
            }
            if let Some(affinity) = requirements.affinity
                && runtime.kind.reasoning_affinity() != Some(affinity)
            {
                capability_failures.push(format!("{target}: opaque reasoning requires {affinity}"));
                continue;
            }
            let capabilities = self
                .config
                .models
                .get(&target)
                .and_then(|model| model.capabilities)
                .unwrap_or_else(|| runtime.config.effective_capabilities());
            let missing = capabilities.missing(&requirements);
            if !missing.is_empty() {
                capability_failures.push(format!("{target}: missing {}", missing.join(", ")));
                continue;
            }
            if !request.extra.is_empty() && !runtime.config.allow_unknown_fields {
                capability_failures.push(format!(
                    "{target}: unsupported request fields {}",
                    request.extra.keys().cloned().collect::<Vec<_>>().join(", ")
                ));
                continue;
            }
            let Some(circuit_permit) = runtime.breaker.allow() else {
                last_retryable = Some(RouterError::unavailable(format!(
                    "circuit is open for {target}"
                )));
                continue;
            };
            attempts += 1;
            let selected_key = upstream_override.as_ref().or(runtime.server_key.as_deref());
            if selected_key.is_none() {
                runtime.breaker.failure(circuit_permit);
                last_retryable = Some(RouterError::unavailable(format!(
                    "provider {} has no configured API key",
                    runtime.provider
                )));
                continue;
            }
            // The first-event budget includes waiting for HTTP response headers.
            // An adapter may not yield a stream until headers arrive; timing only
            // stream.next() lets a busy upstream hold a request indefinitely.
            let deadline = tokio::time::Instant::now()
                + Duration::from_millis(self.config.server.first_byte_timeout_ms);
            let result = tokio::select! {
                () = cancel.cancelled() => {
                    runtime.breaker.cancel(circuit_permit);
                    return Err(RouterError::new(
                        http::StatusCode::REQUEST_TIMEOUT, "request cancelled", "request_error", "request_cancelled",
                    ));
                },
                result = tokio::time::timeout_at(deadline, runtime.adapter.stream(
                    &runtime.model, request.clone(), capabilities, selected_key, cancel.child_token(),
                )) => result.unwrap_or_else(|_| Err(RouterError::timeout(format!(
                    "provider {} did not produce response headers before the deadline", runtime.provider,
                )))),
            };
            let mut stream = match result {
                Ok(stream) => stream,
                Err(error)
                    if error.retryable && !error.ambiguous && upstream_override.is_none() =>
                {
                    runtime.breaker.failure(circuit_permit);
                    last_retryable = Some(error);
                    continue;
                }
                Err(error) => {
                    if error.retryable || error.status.is_server_error() {
                        runtime.breaker.failure(circuit_permit);
                    } else {
                        runtime.breaker.success(circuit_permit);
                    }
                    return Err(error);
                }
            };
            let first = tokio::select! {
                () = cancel.cancelled() => {
                    runtime.breaker.cancel(circuit_permit);
                    return Err(RouterError::new(
                        http::StatusCode::REQUEST_TIMEOUT, "request cancelled", "request_error", "request_cancelled",
                    ));
                },
                first = tokio::time::timeout_at(deadline, stream.next()) => first,
            };
            let first = match first {
                Ok(Some(Ok(event))) => event,
                Ok(Some(Err(error)))
                    if error.retryable && !error.ambiguous && upstream_override.is_none() =>
                {
                    runtime.breaker.failure(circuit_permit);
                    last_retryable = Some(error);
                    continue;
                }
                Ok(Some(Err(error))) => {
                    if error.retryable || error.status.is_server_error() {
                        runtime.breaker.failure(circuit_permit);
                    } else {
                        runtime.breaker.success(circuit_permit);
                    }
                    return Err(error);
                }
                Ok(None) => {
                    runtime.breaker.failure(circuit_permit);
                    last_retryable = Some(RouterError::upstream(
                        http::StatusCode::BAD_GATEWAY,
                        format!("provider {} returned an empty stream", runtime.provider),
                    ));
                    continue;
                }
                Err(_) => {
                    runtime.breaker.failure(circuit_permit);
                    return Err(RouterError::timeout(format!(
                        "provider {} did not produce a first event before the deadline",
                        runtime.provider
                    )));
                }
            };
            runtime.breaker.success(circuit_permit);
            let breaker = Arc::clone(&runtime.breaker);
            let wrapped = Box::pin(try_stream! {
                yield first;
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(event) => yield event,
                        Err(error) => {
                            breaker.stream_failure();
                            Err(error)?;
                        }
                    }
                }
            });
            return Ok(RoutedStream {
                requested_model,
                provider: runtime.provider,
                model: runtime.model,
                metric_model: if self.config.models.contains_key(&target) {
                    split_target(&target)?.1.to_owned()
                } else {
                    "_undeclared".into()
                },
                attempts,
                exclude_reasoning,
                stream: wrapped,
            });
        }
        if !capability_failures.is_empty() && last_retryable.is_none() {
            return Err(RouterError::invalid(
                format!(
                    "no route can preserve the requested features: {}",
                    capability_failures.join("; ")
                ),
                "model",
            ));
        }
        Err(last_retryable
            .unwrap_or_else(|| RouterError::unavailable("all configured routes are unavailable")))
    }

    fn target_runtime(&self, target: &str) -> Result<TargetRuntime, RouterError> {
        let (provider, model) = split_target(target)?;
        let config =
            self.config.providers.get(provider).ok_or_else(|| {
                RouterError::invalid(format!("unknown provider {provider}"), "model")
            })?;
        let server_key = self.provider_keys.get(provider).cloned();
        let adapter = self.adapters.get(provider).cloned().ok_or_else(|| {
            RouterError::unavailable(format!("provider adapter {provider} is unavailable"))
        })?;
        // Undeclared model IDs intentionally share a provider-default breaker.
        // This keeps attacker-controlled model strings from growing router
        // state while declared routes retain independent circuit state.
        let breaker_key = if self.config.models.contains_key(target) {
            target.to_owned()
        } else {
            format!("{provider}/*")
        };
        let breaker = self
            .breakers
            .get(&breaker_key)
            .map(|entry| Arc::clone(entry.value()))
            .ok_or_else(|| RouterError::unavailable("provider circuit is unavailable"))?;
        Ok(TargetRuntime {
            provider: provider.into(),
            model: model.into(),
            kind: config.kind,
            config: config.clone(),
            server_key,
            adapter,
            breaker,
        })
    }
}

pub struct AuthStore {
    authentication_required: bool,
    bootstrap_digests: Vec<[u8; 32]>,
    runtime_keys: DashMap<String, RuntimeRouterKey>,
    runtime_key_management: Mutex<()>,
    max_runtime_keys: usize,
    pub allow_upstream_key_header: bool,
}

struct RuntimeRouterKey {
    digest: [u8; 32],
    label: String,
    created_at: u64,
    last_used_at: AtomicU64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeRouterKeyInfo {
    pub id: String,
    pub label: String,
    pub created_at: u64,
    pub last_used_at: Option<u64>,
}

impl AuthStore {
    pub fn from_config(config: &AppConfig) -> Result<Self, RouterError> {
        let mut digests = Vec::with_capacity(config.auth.router_key_envs.len());
        let authentication_required = config.auth.requires_key();
        if authentication_required && config.auth.router_key_envs.is_empty() {
            return Err(RouterError::invalid(
                "authentication requires at least one router key",
                "auth.router_key_envs",
            ));
        }
        for name in config
            .auth
            .router_key_envs
            .iter()
            .filter(|_| authentication_required)
        {
            let key = std::env::var(name).map_err(|_| {
                RouterError::invalid(
                    format!("required secret environment variable {name} is not set"),
                    "auth.router_key_envs",
                )
            })?;
            digests.push(digest(&key));
        }
        Ok(Self {
            authentication_required,
            bootstrap_digests: digests,
            runtime_keys: DashMap::new(),
            runtime_key_management: Mutex::new(()),
            max_runtime_keys: config.web_ui.max_runtime_keys,
            allow_upstream_key_header: config.auth.allow_upstream_key_header,
        })
    }

    #[must_use]
    pub fn from_keys<'a>(
        keys: impl IntoIterator<Item = &'a str>,
        allow_upstream_key_header: bool,
    ) -> Self {
        Self {
            authentication_required: true,
            bootstrap_digests: keys.into_iter().map(digest).collect(),
            runtime_keys: DashMap::new(),
            runtime_key_management: Mutex::new(()),
            max_runtime_keys: 64,
            allow_upstream_key_header,
        }
    }

    #[must_use]
    pub fn authentication_required(&self) -> bool {
        self.authentication_required
    }

    #[must_use]
    pub fn authenticate(&self, candidate: &str) -> bool {
        let candidate = digest(candidate);
        let mut matched = false;
        for expected in &self.bootstrap_digests {
            matched |= bool::from(expected.ct_eq(&candidate));
        }
        let now = unix_time();
        for record in &self.runtime_keys {
            let key_matches = bool::from(record.digest.ct_eq(&candidate));
            if key_matches {
                record.last_used_at.store(now, Ordering::Relaxed);
            }
            matched |= key_matches;
        }
        matched
    }

    pub fn add_runtime_key(
        &self,
        id: String,
        label: String,
        secret: &str,
    ) -> Result<(), RouterError> {
        let _management = self
            .runtime_key_management
            .lock()
            .expect("runtime key management lock poisoned");
        if self.runtime_keys.len() >= self.max_runtime_keys {
            return Err(RouterError::new(
                http::StatusCode::CONFLICT,
                "runtime router key limit reached",
                "invalid_request_error",
                "runtime_key_limit",
            ));
        }
        if self.runtime_keys.contains_key(&id) {
            return Err(RouterError::new(
                http::StatusCode::CONFLICT,
                "runtime router key identifier already exists",
                "invalid_request_error",
                "runtime_key_exists",
            ));
        }
        self.runtime_keys.insert(
            id,
            RuntimeRouterKey {
                digest: digest(secret),
                label,
                created_at: unix_time(),
                last_used_at: AtomicU64::new(0),
            },
        );
        Ok(())
    }

    #[must_use]
    pub fn remove_runtime_key(&self, id: &str) -> bool {
        let _management = self
            .runtime_key_management
            .lock()
            .expect("runtime key management lock poisoned");
        self.runtime_keys.remove(id).is_some()
    }

    #[must_use]
    pub fn runtime_keys(&self) -> Vec<RuntimeRouterKeyInfo> {
        let mut keys = self
            .runtime_keys
            .iter()
            .map(|record| {
                let last_used_at = record.last_used_at.load(Ordering::Relaxed);
                RuntimeRouterKeyInfo {
                    id: record.key().clone(),
                    label: record.label.clone(),
                    created_at: record.created_at,
                    last_used_at: (last_used_at != 0).then_some(last_used_at),
                }
            })
            .collect::<Vec<_>>();
        keys.sort_unstable_by(|left, right| right.created_at.cmp(&left.created_at));
        keys
    }

    #[must_use]
    pub const fn bootstrap_key_count(&self) -> usize {
        self.bootstrap_digests.len()
    }

    #[must_use]
    pub const fn max_runtime_keys(&self) -> usize {
        self.max_runtime_keys
    }
}

fn digest(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::AuthStore;

    #[tokio::test]
    async fn customer_key_failures_do_not_poison_shared_provider_circuits() {
        use super::*;
        use secrecy::ExposeSecret;
        struct Keys;
        #[async_trait]
        impl ProviderAdapter for Keys {
            async fn stream(
                &self,
                _: &str,
                _: ChatCompletionRequest,
                _: crate::Capabilities,
                key: Option<&SecretString>,
                _: CancellationToken,
            ) -> Result<EventStream, RouterError> {
                if key.is_some_and(|k| k.expose_secret() == "customer-limited") {
                    return Err(RouterError::upstream(
                        http::StatusCode::TOO_MANY_REQUESTS,
                        "provider quota exhausted",
                    ));
                }
                Ok(Box::pin(futures::stream::iter([Ok(
                    ProviderEvent::TextDelta {
                        index: 0,
                        text: "ok".into(),
                    },
                )])))
            }
        }
        let config: AppConfig=serde_json::from_value(serde_json::json!({"providers":{"test":{"kind":"openai-compatible","base_url":"https://example.test/v1"}}})).unwrap();
        let router = Router::with_provider_keys(
            config,
            HashMap::from([("test".into(), Arc::new(Keys) as Arc<dyn ProviderAdapter>)]),
            HashMap::from([("test".into(), SecretString::from("system-key"))]),
        )
        .unwrap();
        let request: ChatCompletionRequest = serde_json::from_value(
            serde_json::json!({"model":"test/model","messages":[{"role":"user","content":"hi"}]}),
        )
        .unwrap();
        for _ in 0..5 {
            assert!(
                router
                    .route(
                        request.clone(),
                        Some(SecretString::from("customer-limited")),
                        CancellationToken::new()
                    )
                    .await
                    .is_err()
            );
        }
        assert!(
            router
                .circuit_states()
                .iter()
                .all(|(_, state)| *state == CircuitState::Closed)
        );
        assert!(
            router
                .route(request, None, CancellationToken::new())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn first_event_deadline_and_cancellation_cover_response_headers() {
        use super::*;
        struct Waiting {
            headers: bool,
        }
        #[async_trait]
        impl ProviderAdapter for Waiting {
            async fn stream(
                &self,
                _: &str,
                _: ChatCompletionRequest,
                _: crate::Capabilities,
                _: Option<&SecretString>,
                _: CancellationToken,
            ) -> Result<EventStream, RouterError> {
                if self.headers {
                    std::future::pending::<()>().await;
                }
                Ok(Box::pin(futures::stream::pending()))
            }
        }
        for (headers, cancelled) in [(true, false), (false, false), (true, true), (false, true)] {
            let config: AppConfig = serde_json::from_value(serde_json::json!({
                "auth":{"router_key_envs":["TEST_KEY"]},
                "server":{"first_byte_timeout_ms":if cancelled { 5000 } else { 25 }},
                "providers":{"slow":{"kind":"openai-compatible","base_url":"https://example.test/v1"}}
            })).unwrap();
            let router = Router::with_provider_keys(
                config,
                HashMap::from([(
                    "slow".into(),
                    Arc::new(Waiting { headers }) as Arc<dyn ProviderAdapter>,
                )]),
                HashMap::from([("slow".into(), SecretString::from("fixture-key"))]),
            )
            .unwrap();
            let cancel = CancellationToken::new();
            if cancelled {
                let child = cancel.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    child.cancel();
                });
            }
            let request = serde_json::from_value(serde_json::json!({"model":"slow/model", "messages":[{"role":"user","content":"hi"}]})).unwrap();
            let result =
                tokio::time::timeout(Duration::from_secs(1), router.route(request, None, cancel))
                    .await
                    .expect("headers and first-event waits must both be bounded");
            let Err(error) = result else {
                panic!("a waiting provider cannot complete")
            };
            assert_eq!(
                error.body.code,
                if cancelled {
                    "request_cancelled"
                } else {
                    "upstream_timeout"
                }
            );
            assert!(
                !error.retryable,
                "do not duplicate a potentially accepted generation"
            );
        }
    }

    #[tokio::test]
    async fn model_refresh_coalesces_and_preserves_last_success_on_failure() {
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Catalog {
            calls: AtomicUsize,
            result: Mutex<Option<Vec<ModelObject>>>,
        }
        #[async_trait]
        impl ProviderAdapter for Catalog {
            async fn discover_models(
                &self,
                _: &ProviderConfig,
                _: Option<&SecretString>,
            ) -> Option<Vec<ModelObject>> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.result.lock().unwrap().clone()
            }
            async fn stream(
                &self,
                _: &str,
                _: ChatCompletionRequest,
                _: crate::Capabilities,
                _: Option<&SecretString>,
                _: CancellationToken,
            ) -> Result<EventStream, RouterError> {
                unreachable!("model discovery must not invoke chat")
            }
        }
        let model: ModelObject = serde_json::from_value(serde_json::json!({"id":"brand-new-model","object":"model","created":0,"owned_by":"upstream","context_window":1_000_000})).unwrap();
        let adapter = Arc::new(Catalog {
            calls: AtomicUsize::new(0),
            result: Mutex::new(Some(vec![model])),
        });
        let config: AppConfig = serde_json::from_value(serde_json::json!({"auth":{"router_key_envs":["TEST_KEY"]},"providers":{"custom":{"kind":"openai-compatible","base_url":"https://example.test/v1"}}})).unwrap();
        let router = Router::with_provider_keys(
            config,
            HashMap::from([("custom".into(), adapter.clone() as Arc<dyn ProviderAdapter>)]),
            HashMap::new(),
        )
        .unwrap();
        futures::join!(router.refresh_models(), router.refresh_models());
        assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
        assert_eq!(router.models(0)[0].id, "custom/brand-new-model");
        assert_eq!(router.models(0)[0].metadata["context_window"], 1_000_000);
        *adapter.result.lock().unwrap() = None;
        *router.model_refresh.lock().await = None;
        router.refresh_models().await;
        assert_eq!(
            router.models(0).len(),
            1,
            "transient errors retain the last complete catalog"
        );
        *adapter.result.lock().unwrap() = Some(Vec::new());
        *router.model_refresh.lock().await = None;
        router.refresh_models().await;
        assert!(
            router.models(0).is_empty(),
            "successful empty catalog removes discovered models"
        );
    }

    #[test]
    fn authenticates_digest_without_storing_plaintext() {
        let store = AuthStore::from_keys(["router-secret"], false);
        assert!(store.authenticate("router-secret"));
        assert!(!store.authenticate("provider-secret"));
    }

    #[test]
    fn runtime_keys_are_hashed_listed_and_revocable() {
        let store = AuthStore::from_keys(["bootstrap"], false);
        store
            .add_runtime_key("key_1".into(), "Local UI".into(), "runtime-secret")
            .unwrap();
        assert!(store.authenticate("runtime-secret"));
        assert_eq!(store.runtime_keys()[0].label, "Local UI");
        assert!(store.runtime_keys()[0].last_used_at.is_some());
        assert!(store.remove_runtime_key("key_1"));
        assert!(!store.authenticate("runtime-secret"));
    }
}
