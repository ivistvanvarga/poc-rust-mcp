//! MCP calculator server with optional PostgreSQL-backed calculation history.
//!
//! The binary (`main.rs`) is a thin CLI/transport wrapper around this library; the split exists so
//! integration tests in `tests/` can drive the server's public surface directly.

pub mod db;
pub mod server;
