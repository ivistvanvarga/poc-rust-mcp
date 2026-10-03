//! MCP calculator server with an optional database-backed calculation history.
//!
//! The binary (`main.rs`) is a thin transport wrapper around this library; the split exists so
//! integration tests in `tests/` can drive the server's public surface directly.
//!
//! [`config`] resolves the layered configuration (defaults, optional TOML file, environment,
//! CLI flags) that [`db::Store`] and [`server::Calculator`] are built from.
//!
//! History storage works on PostgreSQL, MySQL/MariaDB and SQLite. The URL's scheme picks the
//! backend and nothing else is configured; see [`db::Dialect`].

pub mod config;
pub mod db;
pub mod server;
