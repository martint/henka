//! The Refactor MCP server binary.

mod mcp;

use std::path::PathBuf;

use clap::Parser;
use refactor_core::{ProjectRegistry, default_config_path};
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

    let handler = RefactorMcp::new(registry);
    let service = handler.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
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
