use std::{
    io::{self, IsTerminal},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, ValueEnum};
use router_core::{AppConfig, AuthStore, ConfigSource, Router, STANDARD_PROVIDERS};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// The file a router started without `--config` uses when it is present.
const DEFAULT_CONFIG_FILE: &str = "config.toml";

#[derive(Parser)]
#[command(
    name = "haarness-router",
    version,
    about = "Stateless OpenAI-compatible LLM router"
)]
struct Args {
    /// TOML configuration file, or - to read TOML from stdin. Without it the
    /// router uses ./config.toml when present, otherwise the provider API keys
    /// set in the environment.
    #[arg(long, value_name = "FILE|-", env = "HAARNESS_ROUTER_CONFIG")]
    config: Option<PathBuf>,

    /// Override a configuration value. Repeat for multiple values.
    #[arg(long = "set", value_name = "TOML_PATH=VALUE")]
    config_overrides: Vec<String>,

    /// Remove a configuration value or section. Repeat for multiple paths.
    #[arg(long = "unset", value_name = "TOML_PATH")]
    config_removals: Vec<String>,

    /// Require a router access key from `HAARNESS_ROUTER_KEY`.
    #[arg(long, conflicts_with = "no_auth")]
    require_key: bool,

    /// Accept model API requests without a router access key.
    #[arg(long, conflicts_with = "require_key")]
    no_auth: bool,

    /// Override the configured listening address, for example 127.0.0.1:9090.
    #[arg(
        long,
        value_name = "IP:PORT",
        env = "HAARNESS_ROUTER_BIND",
        conflicts_with_all = ["host", "port"]
    )]
    bind: Option<SocketAddr>,

    /// Override only the configured listening IP address.
    #[arg(
        long,
        value_name = "IP",
        env = "HAARNESS_ROUTER_HOST",
        conflicts_with = "bind"
    )]
    host: Option<IpAddr>,

    /// Override only the configured listening port.
    #[arg(
        long,
        value_name = "PORT",
        env = "HAARNESS_ROUTER_PORT",
        conflicts_with = "bind"
    )]
    port: Option<u16>,

    /// Log as readable text or as JSON lines. Defaults to text when standard
    /// error is a terminal and JSON otherwise.
    #[arg(
        long,
        value_enum,
        value_name = "FORMAT",
        env = "HAARNESS_ROUTER_LOG_FORMAT"
    )]
    log_format: Option<LogFormat>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let terminal = io::stderr().is_terminal();
    let format = args.log_format.unwrap_or(if terminal {
        LogFormat::Text
    } else {
        LogFormat::Json
    });
    let color = terminal
        && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
        && console_colors();
    init_logging(format, color);

    let mut overrides = args.config_overrides.clone();
    if args.require_key {
        overrides.push("auth.enabled=true".into());
        overrides.push("auth.router_key_envs=['HAARNESS_ROUTER_KEY']".into());
    } else if args.no_auth {
        overrides.push("auth.enabled=false".into());
    }
    let source = config_source(
        args.config.as_deref(),
        Path::new(DEFAULT_CONFIG_FILE).exists(),
    );
    let mut config = match &source {
        ConfigSource::Environment => AppConfig::from_environment(&overrides, &args.config_removals),
        ConfigSource::Stdin => {
            AppConfig::load_with_overrides("-", &overrides, &args.config_removals)
        }
        ConfigSource::File(path) => {
            AppConfig::load_with_overrides(path, &overrides, &args.config_removals)
        }
    }
    .map_err(anyhow::Error::new)?;
    config.server.bind = effective_bind(config.server.bind, &args);
    let bind = config.server.bind;
    let drain = Duration::from_secs(config.server.drain_seconds);
    let auth = AuthStore::from_config(&config).map_err(anyhow::Error::new)?;
    if !config.providers.is_empty() {
        require_system_certificates()?;
    }
    let adapters = router_providers::build_adapters(&config).map_err(anyhow::Error::new)?;
    let summary = Summary::new(&config, &source, auth.authentication_required());
    let router = Router::new(config, adapters).map_err(anyhow::Error::new)?;
    let source = match source {
        ConfigSource::File(path) => {
            ConfigSource::File(std::fs::canonicalize(&path).unwrap_or(path))
        }
        other => other,
    };
    let application = router_server::app(
        router_server::AppState::try_new(router, auth)
            .map_err(anyhow::Error::new)?
            .with_config_source(source),
    );
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|error| bind_error(bind, &error))?;
    let address = listener.local_addr().unwrap_or(bind);
    if format == LogFormat::Text {
        eprint!("{}", summary.render(address, color));
    } else {
        info!(
            bind = %address,
            version = env!("CARGO_PKG_VERSION"),
            config = summary.source_label,
            providers = summary.providers.len(),
            "haarness router listening"
        );
        if summary.providers.is_empty() {
            if summary.source_label == "environment" {
                warn!(
                    "no model providers are set up; set a provider API key such as OPENAI_API_KEY and restart"
                );
            } else {
                warn!("no model providers are configured; add one under [providers] and restart");
            }
        }
    }

    let shutdown = CancellationToken::new();
    let server_shutdown = shutdown.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, application)
            .with_graceful_shutdown(server_shutdown.cancelled_owned())
            .await
    });
    shutdown_signal().await;
    info!(
        drain_seconds = drain.as_secs(),
        "shutdown requested; draining connections"
    );
    shutdown.cancel();
    if let Ok(result) = tokio::time::timeout(drain, server).await {
        result.context("server task failed")??;
    } else {
        warn!("graceful drain deadline elapsed");
    }
    Ok(())
}

#[cfg(windows)]
fn console_colors() -> bool {
    nu_ansi_term::enable_ansi_support().is_ok()
}

#[cfg(not(windows))]
fn console_colors() -> bool {
    true
}

fn init_logging(format: LogFormat, color: bool) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,tower_http=warn".into());
    let builder = tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(filter);
    match format {
        LogFormat::Json => builder
            .json()
            .with_current_span(false)
            .with_span_list(false)
            .init(),
        LogFormat::Text => builder.with_target(false).with_ansi(color).init(),
    }
}

/// An explicit `--config` (or `HAARNESS_ROUTER_CONFIG`) is always used, so a
/// missing file still stops startup. Without one, `./config.toml` is used when
/// it exists, and the environment otherwise.
fn config_source(explicit: Option<&Path>, default_file_exists: bool) -> ConfigSource {
    match explicit {
        Some(path) if path == Path::new("-") => ConfigSource::Stdin,
        Some(path) => ConfigSource::File(path.to_owned()),
        None if default_file_exists => ConfigSource::File(PathBuf::from(DEFAULT_CONFIG_FILE)),
        None => ConfigSource::Environment,
    }
}

/// Linux keeps its trusted roots in a package (ca-certificates) that minimal systems and container images leave
/// out. The HTTPS client providers need then fails to build, which used to stop the router with a bare "builder
/// error" before it listened.
#[cfg(all(unix, not(target_vendor = "apple")))]
fn require_system_certificates() -> Result<()> {
    if rustls_native_certs::load_native_certs().certs.is_empty() {
        anyhow::bail!(
            "No trusted CA certificates were found on this system, so the router cannot reach providers over HTTPS. \
             Install your distribution's ca-certificates package (for example `sudo apt install ca-certificates`), \
             or point SSL_CERT_FILE at a certificate bundle, then start it again."
        );
    }
    Ok(())
}

#[cfg(not(all(unix, not(target_vendor = "apple"))))]
#[allow(clippy::unnecessary_wraps)]
fn require_system_certificates() -> Result<()> {
    Ok(())
}

fn effective_bind(configured: SocketAddr, args: &Args) -> SocketAddr {
    if let Some(bind) = args.bind {
        return bind;
    }
    SocketAddr::new(
        args.host.unwrap_or(configured.ip()),
        args.port.unwrap_or(configured.port()),
    )
}

fn bind_error(bind: SocketAddr, error: &io::Error) -> anyhow::Error {
    let port = bind.port();
    let ip = bind.ip();
    match error.kind() {
        io::ErrorKind::AddrInUse => anyhow!(
            "port {port} on {ip} is already in use, perhaps by another router. Choose another port with --port, for example: haarness-router --port {}",
            port.checked_add(1).unwrap_or(8081)
        ),
        io::ErrorKind::AddrNotAvailable => anyhow!(
            "{ip} is not an address of this machine. Choose another with --host, for example --host 127.0.0.1"
        ),
        io::ErrorKind::PermissionDenied => anyhow!(
            "listening on port {port} was refused: this system reserves it for administrators. Choose another port with --port, for example --port 8080"
        ),
        _ => anyhow!("could not listen on {bind}: {error}"),
    }
}

/// What the startup summary says, gathered before the configuration moves
/// into the router.
struct Summary {
    source_label: &'static str,
    source: String,
    providers: Vec<String>,
    ui_path: Option<String>,
    key_required: bool,
}

impl Summary {
    fn new(config: &AppConfig, source: &ConfigSource, key_required: bool) -> Self {
        Self {
            source_label: source.label(),
            source: match source {
                ConfigSource::File(path) => path.display().to_string(),
                ConfigSource::Stdin => "standard input".into(),
                ConfigSource::Environment => "provider keys in the environment".into(),
            },
            providers: config.providers.keys().cloned().collect(),
            ui_path: config.web_ui.enabled.then(|| config.web_ui.path.clone()),
            key_required,
        }
    }

    fn render(&self, address: SocketAddr, color: bool) -> String {
        let style = |code: &str, text: &str| {
            if color {
                format!("\x1b[{code}m{text}\x1b[0m")
            } else {
                text.to_owned()
            }
        };
        let base = format!("http://{}", display_address(address));
        let mut lines = vec![
            String::new(),
            format!(
                "  {} {}",
                style("1", "Haarness Router"),
                style("2", env!("CARGO_PKG_VERSION"))
            ),
            String::new(),
            format!("  API        {base}/v1"),
        ];
        if let Some(path) = &self.ui_path {
            lines.push(format!("  Dashboard  {base}{path}"));
        }
        if self.providers.is_empty() {
            if self.source_label == "environment" {
                lines.push(format!(
                    "  Providers  {}",
                    style("33", "none yet. Set one of these API keys and restart:")
                ));
                let variables = STANDARD_PROVIDERS
                    .iter()
                    .flat_map(|provider| provider.key_variables.iter().copied())
                    .collect::<Vec<_>>();
                for row in variables.chunks(4) {
                    lines.push(format!("             {}", row.join("  ")));
                }
            } else {
                lines.push(format!(
                    "  Providers  {}",
                    style("33", "none. Add one under [providers] and restart.")
                ));
            }
        } else {
            lines.push(format!("  Providers  {}", self.providers.join(", ")));
        }
        lines.push(format!("  Config     {}", self.source));
        if !address.ip().is_loopback() && !self.key_required {
            lines.push(format!(
                "  {}",
                style(
                    "2",
                    "Reachable from other machines without a router key; --require-key asks for one."
                )
            ));
        }
        lines.push(String::new());
        lines.push(format!("  {}", style("2", "Connect Haarness:")));
        let key = if self.key_required {
            " --key-stdin"
        } else {
            ""
        };
        lines.push(format!("  haarness provider connect-router {base}{key}"));
        lines.push(String::new());
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }
}

/// The address to give people: a wildcard bind is reached on loopback.
fn display_address(address: SocketAddr) -> String {
    let ip = match address.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, address.port()).to_string()
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        net::SocketAddr,
        path::{Path, PathBuf},
    };

    use clap::Parser;
    use router_core::{AppConfig, ConfigSource};

    use super::{Args, LogFormat, Summary, bind_error, config_source, effective_bind};

    fn args() -> Args {
        Args {
            config: None,
            config_overrides: Vec::new(),
            config_removals: Vec::new(),
            require_key: false,
            no_auth: false,
            bind: None,
            host: None,
            port: None,
            log_format: None,
        }
    }

    #[test]
    fn host_and_port_override_independently() {
        let configured: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let mut overrides = args();
        overrides.host = Some("0.0.0.0".parse().unwrap());
        assert_eq!(
            effective_bind(configured, &overrides).to_string(),
            "0.0.0.0:8080"
        );

        overrides.host = None;
        overrides.port = Some(9090);
        assert_eq!(
            effective_bind(configured, &overrides).to_string(),
            "127.0.0.1:9090"
        );
    }

    #[test]
    fn complete_bind_override_wins() {
        let configured: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let mut overrides = args();
        overrides.bind = Some("[::1]:7070".parse().unwrap());
        assert_eq!(
            effective_bind(configured, &overrides).to_string(),
            "[::1]:7070"
        );
    }

    #[test]
    fn an_explicit_configuration_is_used_even_when_missing() {
        assert_eq!(
            config_source(Some(Path::new("missing.toml")), false),
            ConfigSource::File(PathBuf::from("missing.toml"))
        );
        assert_eq!(
            config_source(Some(Path::new("-")), true),
            ConfigSource::Stdin
        );
        assert_eq!(
            config_source(None, true),
            ConfigSource::File(PathBuf::from("config.toml"))
        );
        assert_eq!(config_source(None, false), ConfigSource::Environment);
    }

    #[test]
    fn version_line_is_the_binary_name_and_version() {
        let error = Args::try_parse_from(["haarness-router", "--version"])
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            format!("haarness-router {}\n", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn log_format_accepts_text_and_json_only() {
        let parsed = Args::try_parse_from(["haarness-router", "--log-format", "json"]).unwrap();
        assert_eq!(parsed.log_format, Some(LogFormat::Json));
        assert!(Args::try_parse_from(["haarness-router", "--log-format", "yaml"]).is_err());
    }

    #[test]
    fn a_port_in_use_is_named_with_the_flag_that_fixes_it() {
        let bind: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let message = bind_error(bind, &io::Error::from(io::ErrorKind::AddrInUse)).to_string();
        assert!(
            message.contains("port 8080 on 127.0.0.1 is already in use"),
            "{message}"
        );
        assert!(message.contains("--port 8081"), "{message}");
    }

    fn summary(overrides: &[&str], source: &ConfigSource, key_required: bool) -> Summary {
        let overrides = overrides
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>();
        let config = AppConfig::from_environment(&overrides, &[]).unwrap();
        Summary::new(&config, source, key_required)
    }

    #[test]
    fn summary_lists_addresses_providers_and_the_connect_command() {
        let text = summary(
            &[
                "providers={}",
                "providers.anthropic.kind=anthropic",
                "providers.openai.kind=openai",
            ],
            &ConfigSource::Environment,
            false,
        )
        .render("0.0.0.0:8080".parse().unwrap(), false);
        assert!(text.contains(&format!("Haarness Router {}", env!("CARGO_PKG_VERSION"))));
        assert!(
            text.contains("API        http://127.0.0.1:8080/v1"),
            "{text}"
        );
        assert!(
            text.contains("Dashboard  http://127.0.0.1:8080/ui"),
            "{text}"
        );
        assert!(text.contains("Providers  anthropic, openai"), "{text}");
        assert!(text.contains("Reachable from other machines without a router key"));
        assert!(text.contains("haarness provider connect-router http://127.0.0.1:8080\n"));
        assert!(!text.contains('\x1b'));
    }

    #[test]
    fn summary_without_providers_names_the_variables_to_set() {
        let text = summary(&["providers={}"], &ConfigSource::Environment, true)
            .render("127.0.0.1:18480".parse().unwrap(), false);
        assert!(
            text.contains("none yet. Set one of these API keys"),
            "{text}"
        );
        for variable in [
            "OPENAI_API_KEY",
            "GOOGLE_API_KEY",
            "AWS_BEARER_TOKEN_BEDROCK",
        ] {
            assert!(text.contains(variable), "{variable}: {text}");
        }
        assert!(!text.contains("Reachable from other machines"));
        assert!(
            text.contains("haarness provider connect-router http://127.0.0.1:18480 --key-stdin")
        );

        let file = summary(
            &["providers={}", "web_ui.enabled=false"],
            &ConfigSource::File("router.toml".into()),
            false,
        )
        .render("[::1]:9090".parse().unwrap(), true);
        assert!(file.contains("none. Add one under [providers]"), "{file}");
        assert!(file.contains("Config     router.toml"));
        assert!(file.contains("API        http://[::1]:9090/v1"));
        assert!(!file.contains("Dashboard"));
    }
}
