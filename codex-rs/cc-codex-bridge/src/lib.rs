//! A local bridge that speaks the Codex app-server protocol to a Codex client
//! while an unmodified Claude Code process does the work.
//!
//! The bridge starts `claude` with `--input-format stream-json
//! --output-format stream-json`, writes each turn's prompt to its stdin, and
//! turns the JSON messages it prints into Codex thread, turn and item
//! notifications. It never reads credentials and never calls Anthropic
//! endpoints itself.

pub mod config;
pub mod engine;
pub mod items;
pub mod server;
pub mod session;
pub mod store;
pub mod translator;
pub mod transport;
