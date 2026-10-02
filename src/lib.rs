//! MCP calculator server with optional PostgreSQL-backed calculation history.
//!
//! The binary (`main.rs`) is a thin transport wrapper around this library; the split exists so
//! integration tests in `tests/` can drive the server's public surface directly.
//!
//! [`config`] resolves the layered configuration (defaults, optional TOML file, environment,
//! CLI flags) that [`db::Store`] and [`server::Calculator`] are built from.

pub mod config;
pub mod db;
pub mod server;
