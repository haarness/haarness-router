use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    net::SocketAddr,
    path::Path,
};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::{Capabilities, RouterError};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub web_ui: WebUiConfig,
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelConfig>,
    #[serde(default)]
    pub aliases: BTreeMap<String, AliasConfig>,
}

impl AppConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, RouterError> {
        Self::load_with_overrides(path, &[], &[])
    }

    pub fn load_with_overrides(
        path: impl AsRef<Path>,
        overrides: &[String],
        removals: &[String],
    ) -> Result<Self, RouterError> {
        let path = path.as_ref();
        let source = read_config_source(path)?;
        let mut document: toml::Value = toml::from_str(&source).map_err(|error| {
            RouterError::invalid(format!("invalid TOML configuration: {error}"), "config")
        })?;
        for expression in overrides {
            apply_override(&mut document, expression)?;
        }
        for path in removals {
            remove_override(&mut document, path)?;
        }
        let config: Self = document.try_into().map_err(|error| {
            RouterError::invalid(format!("invalid merged configuration: {error}"), "config")
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), RouterError> {
        if self.auth.requires_key() && self.auth.router_key_envs.is_empty() {
            return Err(RouterError::invalid(
                "auth.router_key_envs must contain at least one environment variable",
                "auth.router_key_envs",
            ));
        }
        if self.auth.requires_key() {
            for environment in &self.auth.router_key_envs {
                require_environment(environment, "auth.router_key_envs")?;
            }
        }
        self.web_ui.validate()?;
        for (name, provider) in &self.providers {
            validate_identifier(name, "provider")?;
            let base_url = provider.effective_base_url()?;
            let parsed = Url::parse(&base_url).map_err(|error| {
                RouterError::invalid(
                    format!("provider {name} has an invalid base URL: {error}"),
                    format!("providers.{name}.base_url"),
                )
            })?;
            if parsed.scheme() != "https" && !provider.allow_insecure_http {
                return Err(RouterError::invalid(
                    format!("provider {name} must use HTTPS unless allow_insecure_http is true"),
                    format!("providers.{name}.base_url"),
                ));
            }
            if let Some(environment) = &provider.api_key_env {
                require_environment(environment, &format!("providers.{name}.api_key_env"))?;
            }
            if let Some(header) = &provider.auth_header {
                http::HeaderName::from_bytes(header.as_bytes()).map_err(|_| {
                    RouterError::invalid(
                        format!("provider {name} has an invalid authentication header"),
                        format!("providers.{name}.auth_header"),
                    )
                })?;
            }
            if provider.auth_scheme.as_ref().is_some_and(|scheme| {
                scheme.is_empty()
                    || !scheme
                        .bytes()
                        .all(|byte| byte.is_ascii_graphic() && byte != b':')
            }) {
                return Err(RouterError::invalid(
                    format!("provider {name} has an invalid authentication scheme"),
                    format!("providers.{name}.auth_scheme"),
                ));
            }
        }
        for model in self.models.keys() {
            let (provider, raw_model) = split_target(model)?;
            if raw_model.is_empty() || !self.providers.contains_key(provider) {
                return Err(RouterError::invalid(
                    format!("model {model} references an unknown provider"),
                    format!("models.{model}"),
                ));
            }
        }
        for (alias, definition) in &self.aliases {
            if alias.contains('/') {
                return Err(RouterError::invalid(
                    format!("alias {alias} cannot contain '/'"),
                    format!("aliases.{alias}"),
                ));
            }
            validate_identifier(alias, "alias")?;
            if definition.targets.is_empty() {
                return Err(RouterError::invalid(
                    format!("alias {alias} has no targets"),
                    format!("aliases.{alias}.targets"),
                ));
            }
            for target in &definition.targets {
                let (provider, raw_model) = split_target(target)?;
                if raw_model.is_empty() || !self.providers.contains_key(provider) {
                    return Err(RouterError::invalid(
                        format!("alias {alias} references invalid target {target}"),
                        format!("aliases.{alias}.targets"),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebUiConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_ui_path")]
    pub path: String,
    #[serde(default)]
    pub basic_auth_username_env: Option<String>,
    #[serde(default)]
    pub basic_auth_password_env: Option<String>,
    #[serde(default = "default_true")]
    pub allow_runtime_key_management: bool,
    #[serde(default = "default_runtime_key_limit")]
    pub max_runtime_keys: usize,
}

impl Default for WebUiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: default_ui_path(),
            basic_auth_username_env: None,
            basic_auth_password_env: None,
            allow_runtime_key_management: true,
            max_runtime_keys: default_runtime_key_limit(),
        }
    }
}

impl WebUiConfig {
    fn validate(&self) -> Result<(), RouterError> {
        if !self.path.starts_with('/')
            || self.path == "/"
            || self.path.ends_with('/')
            || self.path.contains("//")
            || self.path.split('/').any(|segment| segment == "..")
            || !self
                .path
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_'))
        {
            return Err(RouterError::invalid(
                "web_ui.path must be a non-root absolute path without a trailing slash",
                "web_ui.path",
            ));
        }
        if self.max_runtime_keys == 0 || self.max_runtime_keys > 1_024 {
            return Err(RouterError::invalid(
                "web_ui.max_runtime_keys must be between 1 and 1024",
                "web_ui.max_runtime_keys",
            ));
        }
        match (&self.basic_auth_username_env, &self.basic_auth_password_env) {
            (Some(_), Some(_)) if !self.enabled => {}
            (Some(username), Some(password)) => {
                require_environment(username, "web_ui.basic_auth_username_env")?;
                require_environment(password, "web_ui.basic_auth_password_env")?;
            }
            (None, None) => {}
            _ => {
                return Err(RouterError::invalid(
                    "web UI Basic authentication requires both username and password environment variables",
                    "web_ui.basic_auth_username_env",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,
    #[serde(default = "default_body_limit")]
    pub max_request_bytes: usize,
    #[serde(default = "default_concurrency")]
    pub max_concurrent_requests: usize,
    #[serde(default = "default_first_byte_timeout")]
    pub first_byte_timeout_ms: u64,
    #[serde(default = "default_drain_seconds")]
    pub drain_seconds: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            max_request_bytes: default_body_limit(),
            max_concurrent_requests: default_concurrency(),
            first_byte_timeout_ms: default_first_byte_timeout(),
            drain_seconds: default_drain_seconds(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Omitted means no auth for new configs, while legacy key lists stay protected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub router_key_envs: Vec<String>,
    #[serde(default)]
    pub allow_upstream_key_header: bool,
}

impl AuthConfig {
    #[must_use]
    pub fn requires_key(&self) -> bool {
        self.enabled.unwrap_or(!self.router_key_envs.is_empty())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Openai,
    Anthropic,
    Gemini,
    Groq,
    Mistral,
    Xai,
    Deepseek,
    Together,
    Fireworks,
    OpenaiCompatible,
    AmazonBedrock,
}

/// The upstream wire contract used by a provider profile.
///
/// Provider brands and wire protocols are deliberately separate: adding
/// another OpenAI-compatible profile must not add another adapter, while a new
/// native protocol must be classified here and handled exhaustively by the
/// provider factory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderProtocol {
    OpenAiChatCompletions,
    AnthropicMessages,
    GeminiGenerateContent,
    BedrockConverse,
}

#[derive(Clone, Copy)]
pub(crate) enum CapabilityProfile {
    OpenAi,
    Anthropic,
    Gemini,
    ConservativeOpenAiCompatible,
}

#[derive(Clone, Copy)]
struct ProviderDescriptor {
    protocol: ProviderProtocol,
    capability_profile: CapabilityProfile,
    default_base_url: Option<&'static str>,
}

impl ProviderKind {
    const fn descriptor(self) -> ProviderDescriptor {
        match self {
            Self::Openai => ProviderDescriptor {
                protocol: ProviderProtocol::OpenAiChatCompletions,
                capability_profile: CapabilityProfile::OpenAi,
                default_base_url: Some("https://api.openai.com/v1"),
            },
            Self::Anthropic => ProviderDescriptor {
                protocol: ProviderProtocol::AnthropicMessages,
                capability_profile: CapabilityProfile::Anthropic,
                default_base_url: Some("https://api.anthropic.com/v1"),
            },
            Self::Gemini => ProviderDescriptor {
                protocol: ProviderProtocol::GeminiGenerateContent,
                capability_profile: CapabilityProfile::Gemini,
                default_base_url: Some("https://generativelanguage.googleapis.com/v1beta"),
            },
            Self::Groq => ProviderDescriptor {
                protocol: ProviderProtocol::OpenAiChatCompletions,
                capability_profile: CapabilityProfile::ConservativeOpenAiCompatible,
                default_base_url: Some("https://api.groq.com/openai/v1"),
            },
            Self::Mistral => ProviderDescriptor {
                protocol: ProviderProtocol::OpenAiChatCompletions,
                capability_profile: CapabilityProfile::ConservativeOpenAiCompatible,
                default_base_url: Some("https://api.mistral.ai/v1"),
            },
            Self::Xai => ProviderDescriptor {
                protocol: ProviderProtocol::OpenAiChatCompletions,
                capability_profile: CapabilityProfile::ConservativeOpenAiCompatible,
                default_base_url: Some("https://api.x.ai/v1"),
            },
            Self::Deepseek => ProviderDescriptor {
                protocol: ProviderProtocol::OpenAiChatCompletions,
                capability_profile: CapabilityProfile::ConservativeOpenAiCompatible,
                default_base_url: Some("https://api.deepseek.com"),
            },
            Self::Together => ProviderDescriptor {
                protocol: ProviderProtocol::OpenAiChatCompletions,
                capability_profile: CapabilityProfile::ConservativeOpenAiCompatible,
                default_base_url: Some("https://api.together.ai/v1"),
            },
            Self::Fireworks => ProviderDescriptor {
                protocol: ProviderProtocol::OpenAiChatCompletions,
                capability_profile: CapabilityProfile::ConservativeOpenAiCompatible,
                default_base_url: Some("https://api.fireworks.ai/inference/v1"),
            },
            Self::AmazonBedrock => ProviderDescriptor {
                protocol: ProviderProtocol::BedrockConverse,
                capability_profile: CapabilityProfile::ConservativeOpenAiCompatible,
                default_base_url: Some("https://bedrock-runtime.us-east-1.amazonaws.com"),
            },
            Self::OpenaiCompatible => ProviderDescriptor {
                protocol: ProviderProtocol::OpenAiChatCompletions,
                capability_profile: CapabilityProfile::ConservativeOpenAiCompatible,
                default_base_url: None,
            },
        }
    }

    #[must_use]
    pub const fn protocol(self) -> ProviderProtocol {
        self.descriptor().protocol
    }

    pub(crate) const fn capability_profile(self) -> CapabilityProfile {
        self.descriptor().capability_profile
    }

    #[must_use]
    pub const fn is_native_openai(self) -> bool {
        matches!(self, Self::Openai)
    }

    #[must_use]
    pub fn default_base_url(self) -> Option<&'static str> {
        self.descriptor().default_base_url
    }

    #[must_use]
    pub fn reasoning_affinity(self) -> Option<&'static str> {
        if self.is_native_openai() {
            return Some("openai");
        }
        match self.protocol() {
            ProviderProtocol::AnthropicMessages => Some("anthropic"),
            ProviderProtocol::GeminiGenerateContent => Some("google"),
            ProviderProtocol::OpenAiChatCompletions | ProviderProtocol::BedrockConverse => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Custom compatible endpoints may select a provider-specific header.
    /// When omitted, `Authorization: Bearer` is used.
    #[serde(default)]
    pub auth_header: Option<String>,
    /// Optional prefix for a custom authentication header, for example
    /// `Bearer`. A configured custom header without this field receives the
    /// raw secret value.
    #[serde(default)]
    pub auth_scheme: Option<String>,
    #[serde(default)]
    pub capabilities: Option<Capabilities>,
    #[serde(default)]
    pub allow_unknown_fields: bool,
    #[serde(default)]
    pub allow_insecure_http: bool,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_idle_timeout")]
    pub stream_idle_timeout_ms: u64,
    #[serde(default = "default_error_body_limit")]
    pub max_error_body_bytes: usize,
}

impl ProviderConfig {
    pub fn effective_base_url(&self) -> Result<String, RouterError> {
        self.base_url
            .clone()
            .or_else(|| self.kind.default_base_url().map(str::to_owned))
            .ok_or_else(|| RouterError::invalid("custom provider requires base_url", "base_url"))
    }

    #[must_use]
    pub fn effective_capabilities(&self) -> Capabilities {
        self.capabilities
            .unwrap_or_else(|| Capabilities::for_kind(self.kind))
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    #[serde(default)]
    pub capabilities: Option<Capabilities>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AliasConfig {
    pub targets: Vec<String>,
}

pub fn split_target(target: &str) -> Result<(&str, &str), RouterError> {
    target.split_once('/').ok_or_else(|| {
        RouterError::invalid(
            format!("model {target} must be an alias or provider/model"),
            "model",
        )
    })
}

fn read_config_source(path: &Path) -> Result<String, RouterError> {
    if path == Path::new("-") {
        let mut source = String::new();
        io::stdin().read_to_string(&mut source).map_err(|error| {
            RouterError::invalid(
                format!("failed to read configuration from stdin: {error}"),
                "config",
            )
        })?;
        return Ok(source);
    }
    fs::read_to_string(path).map_err(|error| {
        RouterError::invalid(
            format!("failed to read configuration {}: {error}", path.display()),
            "config",
        )
    })
}

fn apply_override(document: &mut toml::Value, expression: &str) -> Result<(), RouterError> {
    if expression.contains(['\n', '\r']) {
        return Err(config_override_error(
            "configuration overrides cannot contain newlines",
        ));
    }
    let (path, raw_value) = expression
        .split_once('=')
        .ok_or_else(|| config_override_error("configuration override must use TOML_PATH=VALUE"))?;
    let segments = parse_key_path(path.trim())?;
    let value = parse_override_value(raw_value.trim())?;
    let mut table = document
        .as_table_mut()
        .ok_or_else(|| config_override_error("configuration document root must be a TOML table"))?;
    for segment in &segments[..segments.len() - 1] {
        let entry = table
            .entry(segment.clone())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        table = entry.as_table_mut().ok_or_else(|| {
            config_override_error(format!("cannot set {expression}: {segment} is not a table"))
        })?;
    }
    table.insert(
        segments.last().expect("override path is non-empty").clone(),
        value,
    );
    Ok(())
}

fn remove_override(document: &mut toml::Value, path: &str) -> Result<(), RouterError> {
    let segments = parse_key_path(path.trim())?;
    let mut table = document
        .as_table_mut()
        .ok_or_else(|| config_override_error("configuration document root must be a TOML table"))?;
    for segment in &segments[..segments.len() - 1] {
        table = table
            .get_mut(segment)
            .and_then(toml::Value::as_table_mut)
            .ok_or_else(|| config_override_error(format!("cannot unset missing path {path}")))?;
    }
    if table
        .remove(segments.last().expect("override path is non-empty"))
        .is_none()
    {
        return Err(config_override_error(format!(
            "cannot unset missing path {path}"
        )));
    }
    Ok(())
}

fn parse_key_path(path: &str) -> Result<Vec<String>, RouterError> {
    if path.is_empty() || path.contains(['\n', '\r']) {
        return Err(config_override_error(
            "configuration path is empty or invalid",
        ));
    }
    let source = format!("{path} = true");
    let table: toml::Table = toml::from_str(&source).map_err(|error| {
        config_override_error(format!("invalid configuration path {path:?}: {error}"))
    })?;
    let mut segments = Vec::new();
    let mut value = toml::Value::Table(table);
    loop {
        let current = value
            .as_table()
            .ok_or_else(|| config_override_error(format!("invalid configuration path {path:?}")))?;
        if current.len() != 1 {
            return Err(config_override_error(format!(
                "invalid configuration path {path:?}"
            )));
        }
        let (segment, next) = current.iter().next().expect("single entry exists");
        segments.push(segment.clone());
        if next.is_table() {
            value = next.clone();
        } else {
            break;
        }
    }
    Ok(segments)
}

fn parse_override_value(raw: &str) -> Result<toml::Value, RouterError> {
    if raw.is_empty() {
        return Err(config_override_error(
            "configuration override value is empty",
        ));
    }
    let source = format!("value = {raw}");
    match toml::from_str::<toml::Table>(&source) {
        Ok(mut parsed) => Ok(parsed.remove("value").expect("value entry exists")),
        Err(error)
            if raw.starts_with(['[', '{', '"', '\'']) || raw.chars().any(char::is_whitespace) =>
        {
            Err(config_override_error(format!(
                "invalid TOML override value {raw:?}: {error}"
            )))
        }
        Err(_) => Ok(toml::Value::String(raw.to_owned())),
    }
}

fn config_override_error(message: impl Into<String>) -> RouterError {
    RouterError::invalid(message, "config_override")
}

fn require_environment(name: &str, param: &str) -> Result<(), RouterError> {
    if name.is_empty() || std::env::var_os(name).is_none() {
        return Err(RouterError::invalid(
            format!("required secret environment variable {name} is not set"),
            param,
        ));
    }
    Ok(())
}

fn validate_identifier(value: &str, kind: &str) -> Result<(), RouterError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(RouterError::invalid(
            format!("invalid {kind} identifier {value:?}"),
            kind,
        ));
    }
    Ok(())
}

fn default_bind() -> SocketAddr {
    "127.0.0.1:8080".parse().expect("static socket address")
}
const fn default_body_limit() -> usize {
    32 * 1024 * 1024
}
const fn default_concurrency() -> usize {
    20_000
}
const fn default_first_byte_timeout() -> u64 {
    120_000
}
const fn default_drain_seconds() -> u64 {
    30
}
const fn default_connect_timeout() -> u64 {
    5_000
}
const fn default_idle_timeout() -> u64 {
    60_000
}
const fn default_error_body_limit() -> usize {
    4_096
}
const fn default_true() -> bool {
    true
}
fn default_ui_path() -> String {
    "/ui".into()
}
const fn default_runtime_key_limit() -> usize {
    64
}

#[cfg(test)]
mod tests {
    use super::{
        ProviderKind, ProviderProtocol, WebUiConfig, apply_override, remove_override, split_target,
    };

    #[test]
    fn auth_is_optional_but_existing_key_lists_stay_protected() {
        use super::AppConfig;
        for auth in [
            serde_json::json!({}),
            serde_json::json!({"enabled":false,"router_key_envs":["HAARNESS_MISSING_UNUSED_ROUTER_KEY"]}),
        ] {
            let config: AppConfig =
                serde_json::from_value(serde_json::json!({"auth":auth,"providers":{}})).unwrap();
            assert!(!config.auth.requires_key());
            config.validate().unwrap();
            assert!(
                !crate::AuthStore::from_config(&config)
                    .unwrap()
                    .authentication_required()
            );
        }
        let no_section: AppConfig =
            serde_json::from_value(serde_json::json!({"providers":{}})).unwrap();
        assert!(!no_section.auth.requires_key());
        for auth in [
            serde_json::json!({"enabled":true}),
            serde_json::json!({"router_key_envs":["HAARNESS_MISSING_REQUIRED_ROUTER_KEY"]}),
        ] {
            let config: AppConfig =
                serde_json::from_value(serde_json::json!({"auth":auth,"providers":{}})).unwrap();
            assert!(config.auth.requires_key());
            assert!(config.validate().is_err());
            assert!(crate::AuthStore::from_config(&config).is_err());
        }
    }

    #[test]
    fn model_id_after_first_slash_is_opaque() {
        assert_eq!(
            split_target("together/meta-llama/Llama-3").unwrap(),
            ("together", "meta-llama/Llama-3")
        );
    }

    #[test]
    fn provider_profiles_have_explicit_wire_protocols() {
        assert_eq!(
            ProviderKind::Anthropic.protocol(),
            ProviderProtocol::AnthropicMessages
        );
        assert_eq!(
            ProviderKind::Gemini.protocol(),
            ProviderProtocol::GeminiGenerateContent
        );
        for profile in [
            ProviderKind::Openai,
            ProviderKind::Groq,
            ProviderKind::Mistral,
            ProviderKind::Xai,
            ProviderKind::Deepseek,
            ProviderKind::Together,
            ProviderKind::Fireworks,
            ProviderKind::OpenaiCompatible,
        ] {
            assert_eq!(profile.protocol(), ProviderProtocol::OpenAiChatCompletions);
        }
        assert!(ProviderKind::Openai.is_native_openai());
        assert!(!ProviderKind::Groq.is_native_openai());
    }

    #[test]
    fn web_ui_rejects_unsafe_paths_and_incomplete_basic_auth() {
        let unsafe_path = WebUiConfig {
            path: "/ui/../admin".into(),
            ..WebUiConfig::default()
        };
        assert!(unsafe_path.validate().is_err());

        let incomplete_auth = WebUiConfig {
            basic_auth_username_env: Some("UI_USER".into()),
            ..WebUiConfig::default()
        };
        assert!(incomplete_auth.validate().is_err());
    }

    #[test]
    fn cli_overrides_support_typed_values_arrays_and_quoted_keys() {
        let mut document: toml::Value = toml::from_str(
            r#"
            [server]
            max_concurrent_requests = 100
            [web_ui]
            enabled = true
            [models."openai/example"]
            [aliases.fast]
            targets = ["openai/example"]
            "#,
        )
        .unwrap();
        apply_override(&mut document, "server.max_concurrent_requests=30000").unwrap();
        apply_override(&mut document, "web_ui.enabled=false").unwrap();
        apply_override(
            &mut document,
            r#"aliases.fast.targets=["openai/example","gemini/example"]"#,
        )
        .unwrap();
        apply_override(
            &mut document,
            r#"models."openai/example".capabilities.reasoning=true"#,
        )
        .unwrap();

        assert_eq!(
            document["server"]["max_concurrent_requests"].as_integer(),
            Some(30_000)
        );
        assert_eq!(document["web_ui"]["enabled"].as_bool(), Some(false));
        assert_eq!(
            document["aliases"]["fast"]["targets"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            document["models"]["openai/example"]["capabilities"]["reasoning"].as_bool(),
            Some(true)
        );
    }

    #[test]
    fn cli_overrides_create_and_remove_sections_and_accept_bare_strings() {
        let mut document: toml::Value = toml::from_str(
            r#"
            [providers.openai]
            kind = "openai"
            [providers.anthropic]
            kind = "anthropic"
            "#,
        )
        .unwrap();
        apply_override(&mut document, "providers.groq.kind=groq").unwrap();
        apply_override(&mut document, "providers.groq.api_key_env=GROQ_API_KEY").unwrap();
        remove_override(&mut document, "providers.anthropic").unwrap();

        assert_eq!(document["providers"]["groq"]["kind"].as_str(), Some("groq"));
        assert_eq!(
            document["providers"]["groq"]["api_key_env"].as_str(),
            Some("GROQ_API_KEY")
        );
        assert!(document["providers"].get("anthropic").is_none());
        assert!(remove_override(&mut document, "providers.missing").is_err());
    }
}
