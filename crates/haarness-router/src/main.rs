use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result};
use clap::Parser;
use router_core::{AppConfig, AuthStore, Router};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "haarness-router",
    version,
    about = "Stateless OpenAI-compatible LLM router"
)]
struct Args {
    /// TOML configuration file, or - to read TOML from stdin.
    #[arg(
        long,
        value_name = "FILE|-",
        env = "HAARNESS_ROUTER_CONFIG",
        default_value = "config.toml"
    )]
    config: PathBuf,

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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,tower_http=warn".into()),
        )
        .with_current_span(false)
        .with_span_list(false)
        .init();

    let args = Args::parse();
    let mut overrides = args.config_overrides.clone();
    if args.require_key {
        overrides.push("auth.enabled=true".into());
        overrides.push("auth.router_key_envs=['HAARNESS_ROUTER_KEY']".into());
    } else if args.no_auth {
        overrides.push("auth.enabled=false".into());
    }
    let mut config =
        AppConfig::load_with_overrides(&args.config, &overrides, &args.config_removals)
            .map_err(anyhow::Error::new)?;
    config.server.bind = effective_bind(config.server.bind, &args);
    let bind = config.server.bind;
    let drain = Duration::from_secs(config.server.drain_seconds);
    let auth = AuthStore::from_config(&config).map_err(anyhow::Error::new)?;
    let adapters = router_providers::build_adapters(&config).map_err(anyhow::Error::new)?;
    let router = Router::new(config, adapters).map_err(anyhow::Error::new)?;
    let application = router_server::app(
        router_server::AppState::try_new(router, auth).map_err(anyhow::Error::new)?,
    );
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind {bind}"))?;
    info!(%bind, "haarness router listening");

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

fn effective_bind(configured: SocketAddr, args: &Args) -> SocketAddr {
    if let Some(bind) = args.bind {
        return bind;
    }
    SocketAddr::new(
        args.host.unwrap_or(configured.ip()),
        args.port.unwrap_or(configured.port()),
    )
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
    use std::{net::SocketAddr, path::PathBuf};

    use super::{Args, effective_bind};

    fn args() -> Args {
        Args {
            config: PathBuf::from("config.toml"),
            config_overrides: Vec::new(),
            config_removals: Vec::new(),
            require_key: false,
            no_auth: false,
            bind: None,
            host: None,
            port: None,
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
}
