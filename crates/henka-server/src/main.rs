//! The Henka MCP server binary.

mod mcp;
mod ops;
mod pathmap;

use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use henka_core::{ProjectRegistry, ProviderRegistry, default_config_path};
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;

use crate::mcp::HenkaMcp;

/// How clients connect to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Transport {
    /// Standard input/output, for a single local client.
    Stdio,
    /// Streamable HTTP, for a hosted multi-client service.
    Http,
}

/// Multi-tenant MCP server for code refactorings.
#[derive(Debug, Parser)]
#[command(name = "henka", version, about)]
struct Cli {
    /// How clients connect.
    #[arg(long, value_enum, default_value_t = Transport::Stdio)]
    transport: Transport,

    /// Address to bind when `--transport http`. Defaults to loopback; pass
    /// `0.0.0.0:<port>` to listen on all interfaces. The server is
    /// unauthenticated, so binding beyond loopback exposes every registered
    /// project to anyone who can reach the port.
    #[arg(long, default_value = "127.0.0.1:8181")]
    bind: String,

    /// Path to the project registry file. Defaults to
    /// `$XDG_CONFIG_HOME/henka/projects.toml`.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Additional `Host` header value to accept on `--transport http`, beyond
    /// the loopback defaults (localhost, 127.0.0.1, ::1). The HTTP transport
    /// rejects other hosts as a DNS-rebinding guard; add the host your client
    /// connects as — e.g. `--allowed-host host.docker.internal` for a client in
    /// a container. A value without a port matches any port. Repeatable, or set
    /// `HENKA_MCP_ALLOWED_HOST` to a space-separated list (handy in a container,
    /// where the command is the image default).
    #[arg(
        long = "allowed-host",
        value_name = "HOST",
        env = "HENKA_MCP_ALLOWED_HOST",
        value_delimiter = ' '
    )]
    allowed_hosts: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(default_config_path);
    tracing::info!(config = %config_path.display(), "loading project registry");
    let registry = ProjectRegistry::load(&config_path)?;
    tracing::info!(projects = registry.len(), "registry loaded");

    let providers = build_providers();
    let handler = HenkaMcp::new(registry, providers);

    match cli.transport {
        Transport::Stdio => {
            let service = handler.serve(stdio()).await?;
            service.waiting().await?;
        }
        Transport::Http => serve_http(handler, &cli.bind, &cli.allowed_hosts).await?,
    }
    Ok(())
}

/// Serve the handler over streamable HTTP at `/mcp`, one MCP session per client.
async fn serve_http(
    handler: HenkaMcp,
    bind: &str,
    allowed_hosts: &[String],
) -> anyhow::Result<()> {
    // Keep rmcp's loopback-only DNS-rebinding guard, extended with any hosts the
    // operator explicitly trusts. Ignore blanks (e.g. an unset
    // HENKA_MCP_ALLOWED_HOST passed through as an empty string).
    let extra: Vec<String> = allowed_hosts
        .iter()
        .map(|h| h.trim())
        .filter(|h| !h.is_empty())
        .map(str::to_string)
        .collect();
    let mut config = StreamableHttpServerConfig::default();
    config.allowed_hosts.extend(extra.iter().cloned());
    if !extra.is_empty() {
        tracing::info!(allowed_hosts = ?config.allowed_hosts, "accepting additional Host headers");
    }

    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        std::sync::Arc::new(LocalSessionManager::default()),
        config,
    );
    let app = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "serving MCP over streamable HTTP at /mcp");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Assemble the language providers. A provider that cannot start (e.g. Java
/// when no jdtls distribution is available) is logged and skipped, so the
/// server still serves the languages that are ready.
fn build_providers() -> ProviderRegistry {
    let mut providers = ProviderRegistry::new();
    match henka_lang_java::JavaProvider::new() {
        Ok(java) => {
            tracing::info!("Java provider ready (jdtls located)");
            providers.register(std::sync::Arc::new(java));
        }
        Err(e) => {
            tracing::warn!(error = %e, "Java provider unavailable; Java operations disabled");
        }
    }
    match henka_lang_rust::RustProvider::new() {
        Ok(rust) => {
            tracing::info!("Rust provider ready (rust-analyzer located)");
            providers.register(std::sync::Arc::new(rust));
        }
        Err(e) => {
            tracing::warn!(error = %e, "Rust provider unavailable; Rust operations disabled");
        }
    }
    match henka_lang_ts::TsProvider::new() {
        Ok(ts) => {
            tracing::info!("TypeScript/JavaScript provider ready (typescript-language-server located)");
            providers.register_for(henka_lang_ts::LANGUAGES, std::sync::Arc::new(ts));
        }
        Err(e) => {
            tracing::warn!(error = %e, "TypeScript/JavaScript provider unavailable; TS/JS operations disabled");
        }
    }
    providers
}

/// Initialize tracing to stderr — stdout is reserved for the MCP stdio channel.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("HENKA_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();
}
