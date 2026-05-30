//! The Refactor MCP server binary.

mod mcp;
mod ops;

use std::path::PathBuf;

use clap::Parser;
use refactor_core::{ProjectRegistry, ProviderRegistry, default_config_path};
use rmcp::ServiceExt;
use rmcp::transport::stdio;

use crate::mcp::RefactorMcp;

/// Multi-tenant MCP server for code refactorings.
#[derive(Debug, Parser)]
#[command(name = "refactor-server", version, about)]
struct Cli {
    /// Path to the project registry file. Defaults to
    /// `$XDG_CONFIG_HOME/refactor-mcp/projects.toml`.
    #[arg(long)]
    config: Option<PathBuf>,
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
    let handler = RefactorMcp::new(registry, providers);
    let service = handler.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Assemble the language providers. A provider that cannot start (e.g. Java
/// when no jdtls distribution is available) is logged and skipped, so the
/// server still serves the languages that are ready.
fn build_providers() -> ProviderRegistry {
    let mut providers = ProviderRegistry::new();
    match refactor_lang_java::JavaProvider::new() {
        Ok(java) => {
            tracing::info!("Java provider ready (jdtls located)");
            providers.register(std::sync::Arc::new(java));
        }
        Err(e) => {
            tracing::warn!(error = %e, "Java provider unavailable; Java operations disabled");
        }
    }
    providers
}

/// Initialize tracing to stderr — stdout is reserved for the MCP stdio channel.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("REFACTOR_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();
}
