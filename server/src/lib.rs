//! Samsyn: conflict-free live sync for Obsidian vaults, with sharing between users and
//! servers. This library holds everything the `samsyn` binary does, so the pieces can
//! be reused (and fuzzed) on their own:
//!
//! - [`ticket`], [`ids`], [`limits`]: pure, dependency-light building blocks.
//! - [`storage`]: on-disk layout, registry (SQLite), backups.
//! - [`vault`], [`bridge`], [`presence`]: live CRDT documents and the bridges between them.
//! - [`ws`], [`guest`], [`sharing`], [`blobs`], [`account`], [`app`]: the HTTP/WebSocket server.
//! - [`remote`]: the server-to-server protocol over iroh.
//! - [`client`]: the headless sync client.
//! - [`config`], [`cli`]: command-line surface.

// Handlers hand back a ready `axum::Response` as their error, which is a large type by
// clippy's measure; boxing it would only add noise at every `return r`.
#![allow(clippy::result_large_err)]

/// The version this build reports (`/health`, `--version`). Releases stamp it into
/// Cargo.toml; Docker builds of untagged commits and betas pass `SAMSYN_VERSION` instead
/// (see server/Dockerfile and .github/workflows/docker.yml).
pub const VERSION: &str = match option_env!("SAMSYN_VERSION") {
    Some(v) if !v.is_empty() => v,
    _ => env!("CARGO_PKG_VERSION"),
};

pub mod access;
pub mod account;
pub mod app;
pub mod blobs;
pub mod bridge;
pub mod cli;
pub mod client;
pub mod config;
pub mod guest;
pub mod ids;
pub mod limits;
pub mod presence;
pub mod remote;
pub mod sharing;
pub mod storage;
pub mod ticket;
pub mod vault;
pub mod ws;

pub use app::{serve, AppState, SharedState};
