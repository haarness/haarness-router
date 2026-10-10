//! Opt-in, serialized live-provider smoke tests. This executable never prints
//! credentials or response bodies and enforces a hard provider-call budget.

use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use clap::{Parser, ValueEnum};
use futures::StreamExt;
use router_core::{
    AppConfig, AuthConfig, AuthStore, ModelConfig, ProviderConfig, ProviderKind, Router,
    ServerConfig, WebUiConfig,
};
use secrecy::SecretString;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const HARD_MAX_REQUESTS: usize = 30;
const ROUTER_KEY: &str = "live-smoke-ephemeral-router-key";

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Suite {
    Minimal,
    Full,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Feature {
    Text,
    Tools,
    Image,
    Reasoning,
}

#[derive(Parser)]
#[command(about = "Run capped live OpenAI, Anthropic, and Gemini smoke tests")]
struct Args {
    #[arg(
        long,
        default_value = "/Users/alex/work/stackforward/robowork/robowork-backend-stack/environments/prod/group_vars/all.yml"
    )]
    stackforward_secrets: PathBuf,
    #[arg(long, value_enum, default_value = "minimal")]
    suite: Suite,
    #[arg(long, value_parser = ["openai", "anthropic", "gemini"])]
    only: Option<String>,
    /// Read a one-time Anthropic key as one line from stdin. This is accepted
    /// only with `--only anthropic` and never prints or persists the key.
    #[arg(long)]
    anthropic_key_stdin: bool,
    #[arg(long, value_enum)]
    only_feature: Option<Feature>,
    #[arg(long, default_value_t = 20)]
    max_requests: usize,
    #[arg(long, env = "HAARNESS_E2E_OPENAI_MODEL", default_value = "gpt-5-nano")]
    openai_model: String,
    #[arg(
        long,
        env = "HAARNESS_E2E_ANTHROPIC_MODEL",
        default_value = "claude-haiku-4-5-20251001"
    )]
    anthropic_model: String,
    #[arg(
        long,
        env = "HAARNESS_E2E_GEMINI_MODEL",
        default_value = "gemini-3.5-flash-lite"
    )]
    gemini_model: String,
}

#[derive(Deserialize)]
struct StackforwardSecrets {
    #[serde(rename = "openai_api_key")]
    openai: String,
    #[serde(rename = "claude_api_key")]
    claude: String,
    #[serde(rename = "gemini_api_key")]
    gemini: String,
}

struct Budget {
    used: AtomicUsize,
    max: usize,
}

impl Budget {
    fn take(&self) -> Result<()> {
        let previous = self.used.fetch_add(1, Ordering::SeqCst);
        if previous >= self.max || previous >= HARD_MAX_REQUESTS {
            bail!("live request budget exhausted before another provider call");
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.max_requests == 0 || args.max_requests > HARD_MAX_REQUESTS {
        bail!("--max-requests must be between 1 and {HARD_MAX_REQUESTS}");
    }
    let raw = std::fs::read_to_string(&args.stackforward_secrets)
        .with_context(|| format!("failed to read {}", args.stackforward_secrets.display()))?;
    let mut secrets: StackforwardSecrets =
        serde_yaml::from_str(&raw).context("failed to parse Stackforward secret YAML")?;
    if args.anthropic_key_stdin {
        if args.only.as_deref() != Some("anthropic") {
            bail!("--anthropic-key-stdin requires --only anthropic");
        }
        let mut key = String::new();
        std::io::stdin()
            .read_line(&mut key)
            .context("failed to read Anthropic key from stdin")?;
        while key.ends_with(['\n', '\r']) {
            key.pop();
        }
        if key.is_empty() {
            bail!("Anthropic key read from stdin was empty");
        }
        secrets.claude = key;
    }
    let models = [
        ("openai", args.openai_model.as_str()),
        ("anthropic", args.anthropic_model.as_str()),
        ("gemini", args.gemini_model.as_str()),
    ]
    .into_iter()
    .filter(|(provider, _)| args.only.as_deref().is_none_or(|only| only == *provider))
    .collect::<Vec<_>>();
    let config = live_config(&models);
    let adapters = router_providers::build_adapters(&config).map_err(anyhow::Error::new)?;
    let router = Router::with_provider_keys(
        config,
        adapters,
        HashMap::from([
            ("openai".into(), SecretString::from(secrets.openai)),
            ("anthropic".into(), SecretString::from(secrets.claude)),
            ("gemini".into(), SecretString::from(secrets.gemini)),
        ]),
    )
    .map_err(anyhow::Error::new)?;
    let application = router_server::app(router_server::AppState::new(
        router,
        AuthStore::from_keys([ROUTER_KEY], false),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let shutdown = CancellationToken::new();
    let server_shutdown = shutdown.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, application)
            .with_graceful_shutdown(server_shutdown.cancelled_owned())
            .await
    });
    let client = reqwest::Client::builder().build()?;
    let endpoint = format!("http://{address}/v1/chat/completions");
    let budget = Budget {
        used: AtomicUsize::new(0),
        max: args.max_requests.min(HARD_MAX_REQUESTS),
    };

    if args.only_feature.is_none() || matches!(args.only_feature, Some(Feature::Text)) {
        for &(provider, model) in &models {
            let routed_model = format!("{provider}/{model}");
            nonstream_text(&client, &endpoint, &budget, &routed_model).await?;
            stream_text(&client, &endpoint, &budget, &routed_model).await?;
            println!("{provider}: text stream and non-stream passed");
        }
    }
    if matches!(args.suite, Suite::Full) || args.only_feature.is_some() {
        for &(provider, model) in &models {
            let routed_model = format!("{provider}/{model}");
            if args.only_feature.is_none() || matches!(args.only_feature, Some(Feature::Tools)) {
                tool_round_trip(&client, &endpoint, &budget, &routed_model)
                    .await
                    .with_context(|| format!("{provider} tool round-trip failed"))?;
                println!("{provider}: tool round-trip passed");
            }
            if args.only_feature.is_none() || matches!(args.only_feature, Some(Feature::Image)) {
                image_input(&client, &endpoint, &budget, &routed_model)
                    .await
                    .with_context(|| format!("{provider} image input failed"))?;
                println!("{provider}: image input passed");
            }
            if args.only_feature.is_none() || matches!(args.only_feature, Some(Feature::Reasoning))
            {
                reasoning(&client, &endpoint, &budget, provider, &routed_model)
                    .await
                    .with_context(|| format!("{provider} reasoning failed"))?;
                println!("{provider}: reasoning passed");
            }
        }
    }

    shutdown.cancel();
    server.await??;
    println!(
        "live smoke passed with {} provider requests (hard cap {})",
        budget.used.load(Ordering::SeqCst),
        HARD_MAX_REQUESTS
    );
    Ok(())
}

fn live_config(models: &[(&str, &str)]) -> AppConfig {
    let providers = BTreeMap::from([
        ("openai".into(), provider(ProviderKind::Openai)),
        ("anthropic".into(), provider(ProviderKind::Anthropic)),
        ("gemini".into(), provider(ProviderKind::Gemini)),
    ]);
    let declared_models = models
        .iter()
        .map(|(provider, model)| (format!("{provider}/{model}"), ModelConfig::default()))
        .collect();
    AppConfig {
        server: ServerConfig {
            bind: "127.0.0.1:0".parse().expect("static bind address"),
            first_byte_timeout_ms: 120_000,
            ..ServerConfig::default()
        },
        auth: AuthConfig {
            enabled: None,
            router_key_envs: vec!["programmatic-live-smoke".into()],
            allow_upstream_key_header: false,
            metrics_token_env: None,
        },
        web_ui: WebUiConfig {
            enabled: false,
            ..WebUiConfig::default()
        },
        providers,
        models: declared_models,
        aliases: BTreeMap::new(),
    }
}

fn provider(kind: ProviderKind) -> ProviderConfig {
    ProviderConfig {
        kind,
        base_url: None,
        api_key_env: None,
        auth_header: None,
        auth_scheme: None,
        capabilities: None,
        allow_unknown_fields: kind == ProviderKind::Openai,
        allow_insecure_http: false,
        connect_timeout_ms: 5_000,
        stream_idle_timeout_ms: 120_000,
        max_error_body_bytes: 4_096,
    }
}

async fn send(
    client: &reqwest::Client,
    endpoint: &str,
    budget: &Budget,
    body: Value,
) -> Result<reqwest::Response> {
    budget.take()?;
    let response = client
        .post(endpoint)
        .bearer_auth(ROUTER_KEY)
        .json(&body)
        .send()
        .await
        .context("router request failed")?;
    if !response.status().is_success() {
        let status = response.status();
        let error = response.json::<Value>().await.ok();
        let code = error
            .as_ref()
            .and_then(|value| value.pointer("/error/code"))
            .and_then(Value::as_str)
            .unwrap_or("unknown_error");
        let message = error
            .as_ref()
            .and_then(|value| value.pointer("/error/message"))
            .and_then(Value::as_str)
            .unwrap_or("body suppressed");
        bail!("router returned HTTP {status}: {code}: {message}");
    }
    Ok(response)
}

async fn nonstream_text(
    client: &reqwest::Client,
    endpoint: &str,
    budget: &Budget,
    model: &str,
) -> Result<()> {
    let response = send(
        client,
        endpoint,
        budget,
        json!({
            "model": model,
            "messages": [{"role":"user","content":"Reply with exactly: ok"}],
            "max_completion_tokens": 2048
        }),
    )
    .await?
    .json::<Value>()
    .await?;
    if response
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .is_none()
    {
        bail!("{model} returned no text content");
    }
    if response
        .pointer("/usage/total_tokens")
        .and_then(Value::as_u64)
        == Some(0)
        || response
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            .is_none()
    {
        bail!("{model} returned incomplete usage or finish metadata");
    }
    Ok(())
}

async fn stream_text(
    client: &reqwest::Client,
    endpoint: &str,
    budget: &Budget,
    model: &str,
) -> Result<()> {
    let mut stream = send(
        client,
        endpoint,
        budget,
        json!({
            "model": model,
            "messages": [{"role":"user","content":"Reply with exactly: ok"}],
            "max_completion_tokens": 2048,
            "stream": true,
            "stream_options": {"include_usage": true}
        }),
    )
    .await?
    .bytes_stream();
    let mut received = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        received.extend_from_slice(&chunk);
        if received.windows(6).any(|window| window == b"[DONE]") {
            break;
        }
    }
    if !received.windows(6).any(|window| window == b"[DONE]") {
        bail!("{model} stream did not terminate with [DONE]");
    }
    let received = String::from_utf8_lossy(&received);
    if !received.contains("\"content\":") || !received.contains("\"usage\":") {
        bail!("{model} stream did not contain text and usage chunks");
    }
    Ok(())
}

async fn tool_round_trip(
    client: &reqwest::Client,
    endpoint: &str,
    budget: &Budget,
    model: &str,
) -> Result<()> {
    let user = json!({"role":"user","content":"Use the weather tool for Tokyo."});
    let mut first_request = json!({
        "model": model,
        "messages": [user.clone()],
        "tools": [{"type":"function","function":{"name":"weather","description":"Get weather","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}],
        "tool_choice": {"type":"function","function":{"name":"weather"}},
        "max_completion_tokens": 2048
    });
    if model.starts_with("openai/") {
        first_request["reasoning_effort"] = json!("low");
    }
    if model.starts_with("anthropic/claude-fable-5-1")
        || model.starts_with("anthropic/claude-mythos-5-1")
    {
        // These models reject forced tool selection; still require an actual
        // tool call and a successful continuation in the assertions below.
        first_request["tool_choice"] = json!("auto");
    }
    let first = send(client, endpoint, budget, first_request)
        .await?
        .json::<Value>()
        .await?;
    let assistant = first
        .pointer("/choices/0/message")
        .cloned()
        .ok_or_else(|| anyhow!("{model} returned no assistant tool message"))?;
    let call_id = assistant
        .pointer("/tool_calls/0/id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{model} returned no tool call"))?;
    if assistant
        .pointer("/tool_calls/0/function/name")
        .and_then(Value::as_str)
        != Some("weather")
    {
        bail!("{model} returned the wrong tool name");
    }
    let arguments = assistant
        .pointer("/tool_calls/0/function/arguments")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{model} returned no tool arguments"))?;
    let arguments: Value = serde_json::from_str(arguments)
        .with_context(|| format!("{model} returned malformed tool arguments"))?;
    if arguments
        .get("city")
        .and_then(Value::as_str)
        .is_none_or(|city| !city.eq_ignore_ascii_case("tokyo"))
    {
        bail!("{model} returned the wrong city argument");
    }
    let mut second_request = json!({
        "model": model,
        "messages": [user, assistant, {"role":"tool","tool_call_id":call_id,"content":"sunny, 25 C"}],
        "tools": [{"type":"function","function":{"name":"weather","description":"Get weather","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}],
        "max_completion_tokens": 2048
    });
    if model.starts_with("openai/") {
        second_request["reasoning_effort"] = json!("low");
    }
    let second = send(client, endpoint, budget, second_request)
        .await?
        .json::<Value>()
        .await?;
    if second
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .is_none()
    {
        bail!("{model} returned no text after the tool result");
    }
    Ok(())
}

async fn image_input(
    client: &reqwest::Client,
    endpoint: &str,
    budget: &Budget,
    model: &str,
) -> Result<()> {
    let png = test_png()?;
    let mut image_request = json!({
        "model": model,
        "messages": [{"role":"user","content":[
            {"type":"text","text":"Briefly acknowledge that an image was supplied."},
            {"type":"image_url","image_url":{"url":format!("data:image/png;base64,{png}")}}
        ]}],
        "max_completion_tokens": 2048
    });
    if model.starts_with("openai/") {
        image_request["reasoning_effort"] = json!("low");
    }
    let response = send(client, endpoint, budget, image_request)
        .await?
        .json::<Value>()
        .await?;
    if response
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .is_none()
    {
        bail!("{model} returned no image response");
    }
    Ok(())
}

fn test_png() -> Result<String> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 16, 16);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        let mut pixels = Vec::with_capacity(16 * 16 * 3);
        for y in 0..16 {
            for x in 0..16 {
                pixels.extend_from_slice(&[x * 16, y * 16, 128]);
            }
        }
        writer.write_image_data(&pixels)?;
    }
    Ok(STANDARD.encode(bytes))
}

async fn reasoning(
    client: &reqwest::Client,
    endpoint: &str,
    budget: &Budget,
    provider: &str,
    model: &str,
) -> Result<()> {
    // Effort works for both manual Haiku thinking and adaptive current Claude
    // models; Fable/Mythos reject the old explicit manual-token-budget shape.
    let adaptive_claude = [
        "claude-fable-",
        "claude-mythos-",
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-haiku-5",
    ]
    .iter()
    .any(|family| model.contains(family));
    let reasoning = json!({"enabled":true,"effort":if adaptive_claude { "high" } else { "low" }});
    let max_tokens = if adaptive_claude { 4096 } else { 2048 };
    // Adaptive models legitimately skip thinking on trivial multiplication.
    // Exercise real reasoning so the signed-history assertion is meaningful.
    let user = json!({"role":"user","content":"Find the smallest positive integer n with n mod 17 = 11, n mod 19 = 7, and n mod 23 = 5. Verify all three remainders before answering. Give only n."});
    let response = send(
        client,
        endpoint,
        budget,
        json!({
            "model": model,
            "messages": [user.clone()],
            "reasoning": reasoning.clone(),
            "max_completion_tokens": max_tokens
        }),
    )
    .await?
    .json::<Value>()
    .await?;
    if response
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .is_none()
    {
        bail!("{model} returned no reasoning response");
    }
    if provider != "anthropic" {
        return Ok(());
    }

    let assistant = response
        .pointer("/choices/0/message")
        .cloned()
        .ok_or_else(|| anyhow!("{model} returned no reasoning assistant message"))?;
    let details = assistant
        .get("reasoning_details")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("{model} returned no signed reasoning details"))?;
    if details.is_empty()
        || details.iter().any(|detail| {
            detail
                .get("signature")
                .or_else(|| detail.get("data"))
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        })
    {
        bail!("{model} returned incomplete signed reasoning details");
    }
    let continuation = send(
        client,
        endpoint,
        budget,
        json!({
            "model": model,
            "messages": [
                user,
                assistant,
                {"role":"user","content":"Add 1 to that result. Give only the answer."}
            ],
            "reasoning": reasoning,
            "max_completion_tokens": max_tokens
        }),
    )
    .await?
    .json::<Value>()
    .await?;
    if continuation
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .is_none()
    {
        bail!("{model} returned no text after signed reasoning continuation");
    }
    Ok(())
}
