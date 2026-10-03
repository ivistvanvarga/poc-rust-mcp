use std::process::ExitCode;

use anyhow::{Context, Result};
use poc_rust_mcp::{
    config::{Cli, Config, ServerConfig, SystemEnv},
    db::{Dialect, Store},
    server::Calculator,
};
use rmcp::{
    ServiceExt,
    transport::{sse_server::SseServer, stdio},
};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(&error),
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse_from(std::env::args().skip(1))?;
    if cli.help {
        println!("{}", Cli::usage());
        return Ok(());
    }

    // Defaults < config file < environment < flags. Tracing is initialised only after this
    // resolves, because the filter is itself configuration and a bad filter has to be reported
    // before the subscriber takes over stderr.
    let config = Config::resolve(&cli, &SystemEnv)?;
    init_tracing(&config.server.log_filter);

    let store = Store::from_settings(&config.storage);
    if cli.serve_sse {
        serve_sse(&config.server, store).await
    } else {
        serve_stdio(&config.server, store).await
    }
}

fn fail(error: &anyhow::Error) -> ExitCode {
    // Plain stderr, and the whole chain: `anyhow`'s Display is only the outermost context, which on
    // its own would hide why a bind or parse failed.
    eprintln!("poc-rust-mcp: {error}");
    for cause in error.chain().skip(1) {
        eprintln!("  caused by: {cause}");
    }
    ExitCode::FAILURE
}

fn init_tracing(log_filter: &str) {
    tracing_subscriber::fmt()
        // stdout carries JSON-RPC in stdio mode; anything logged there corrupts the stream.
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::new(log_filter))
        .init();
}

async fn serve_stdio(server: &ServerConfig, store: Store) -> Result<()> {
    let storage = store.dialect().map_or("disabled", Dialect::label);
    let service = calculator(server, store)
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

async fn serve_sse(server: &ServerConfig, store: Store) -> Result<()> {
    let addr = server.sse_address;
    let storage = store.dialect().map_or("disabled", Dialect::label);
    let page_size = server.list_page_size;
    let cancel = SseServer::serve(addr)
        .await
        .with_context(|| format!("failed to bind SSE server to {addr}"))?
        // A fresh `Calculator` per session, so each client's subscriptions and log level stay its
        // own: rmcp installs that session's peer on it.
        .with_service(move || Calculator::with_list_page_size(store.clone(), page_size));
    tracing::info!(%addr, storage, sse_path = "/sse", post_path = "/message", "sse server ready");

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for ctrl-c")?;
    tracing::info!("shutting down");
    cancel.cancel();
    Ok(())
}

/// The one place a [`Calculator`] is built, so the page size is never forgotten on one transport.
fn calculator(server: &ServerConfig, store: Store) -> Calculator {
    Calculator::with_list_page_size(store, server.list_page_size)
}
