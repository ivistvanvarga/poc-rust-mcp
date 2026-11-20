// SPDX-License-Identifier: BSD-3-Clause
use std::process::ExitCode;

use anyhow::{Context, Result};
use poc_rust_mcp::{
    config::{Cli, Config, ServerConfig, SystemEnv},
    db::{Dialect, Store},
    server::Calculator,
};
use rmcp::{ServiceExt, transport::stdio};
use tokio_util::sync::CancellationToken;
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
    let storage = store.dialect().map_or("disabled", Dialect::label);
    let settings = poc_rust_mcp::sse::Settings::from_config(server)?;
    let token_required = settings.requires_token();
    let listener = tokio::net::TcpListener::bind(server.sse_address)
        .await
        .with_context(|| format!("failed to bind SSE server to {}", server.sse_address))?;
    let addr = listener
        .local_addr()
        .context("cannot read the SSE listener address")?;
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    });
    let page_size = server.list_page_size;
    tracing::info!(
        %addr,
        storage,
        token_required,
        sse_path = "/sse",
        post_path = "/message",
        "sse server ready"
    );

    // A fresh `Calculator` per session, so each client's subscriptions and log level stay its own;
    // the peer for notifications is picked up from the request context on the first tool call.
    poc_rust_mcp::sse::serve(
        listener,
        settings,
        move || Calculator::with_list_page_size(store.clone(), page_size),
        cancel,
    )
    .await
    .context("SSE server failed")
}

/// The one place a [`Calculator`] is built, so the page size is never forgotten on one transport.
fn calculator(server: &ServerConfig, store: Store) -> Calculator {
    Calculator::with_list_page_size(store, server.list_page_size)
}
