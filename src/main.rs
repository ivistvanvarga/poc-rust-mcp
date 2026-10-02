mod db;
mod server;

use std::{net::SocketAddr, process::ExitCode};

use anyhow::{Context, bail};
use db::Store;
use rmcp::{
    ServiceExt,
    transport::{sse_server::SseServer, stdio},
};
use server::Calculator;
use tracing_subscriber::EnvFilter;

const DEFAULT_SSE_ADDR: &str = "127.0.0.1:8000";
const USAGE: &str = "\
poc-rust-mcp — MCP calculator server with PostgreSQL-backed history

Usage:
  poc-rust-mcp                Serve MCP over stdio (default, for client subprocesses)
  poc-rust-mcp --sse [ADDR]   Serve MCP over SSE on ADDR (default 127.0.0.1:8000)
  poc-rust-mcp --help         Show this message

Environment:
  DATABASE_URL   PostgreSQL URL for calculation history, e.g.
                 postgres://mcp:mcp@127.0.0.1:5432/mcp
                 Unset or unreachable = storage disabled, calculator tools still work
  RUST_LOG       Log filter, e.g. RUST_LOG=debug (logs go to stderr)
";

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "server exited with an error");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

async fn run() -> anyhow::Result<()> {
    let store = Store::from_env();
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None => serve_stdio(store).await,
        Some("--help" | "-h") => {
            println!("{USAGE}");
            Ok(())
        }
        Some("--sse") => {
            let addr = args.next().unwrap_or_else(|| DEFAULT_SSE_ADDR.to_owned());
            serve_sse(addr, store).await
        }
        Some(unknown) => bail!("unknown argument {unknown:?}, try --help"),
    }
}

async fn serve_stdio(store: Store) -> anyhow::Result<()> {
    let storage = if store.is_configured() {
        "postgres"
    } else {
        "disabled"
    };
    let service = Calculator::new(store)
        .serve(stdio())
        .await
        .context("failed to serve over stdio")?;
    tracing::info!(storage, "stdio server ready");
    service
        .waiting()
        .await
        .context("stdio server task failed")?;
    Ok(())
}

async fn serve_sse(addr: String, store: Store) -> anyhow::Result<()> {
    let addr: SocketAddr = addr
        .parse()
        .with_context(|| format!("invalid SSE bind address {addr:?}"))?;
    tracing::info!(
        storage = if store.is_configured() {
            "postgres"
        } else {
            "disabled"
        }
    );
    let cancel = SseServer::serve(addr)
        .await
        .with_context(|| format!("failed to bind SSE server to {addr}"))?
        .with_service(move || Calculator::new(store.clone()));
    tracing::info!(%addr, sse_path = "/sse", post_path = "/message", "sse server ready");

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for ctrl-c")?;
    tracing::info!("shutting down");
    cancel.cancel();
    Ok(())
}
