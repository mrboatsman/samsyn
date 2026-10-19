use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug, Clone)]
#[command(name = "samsyn-server", version = crate::VERSION, about = "CRDT sync server for the Samsyn Obsidian plugin")]
pub struct Cli {
    /// Directory holding all server data (registry, vaults, blobs, snapshots).
    #[arg(long, global = true, env = "SAMSYN_DATA_DIR", default_value = "./data")]
    pub data_dir: PathBuf,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Run the sync server (default when no subcommand is given).
    Serve(ServeArgs),
    /// Run as a headless client: keep a local directory in sync with a vault on a server.
    Sync(crate::client::SyncArgs),
    /// Manage users and their tokens.
    User {
        #[command(subcommand)]
        cmd: UserCmd,
    },
    /// Inspect vaults.
    Vault {
        #[command(subcommand)]
        cmd: VaultCmd,
    },
    /// Write a full-state snapshot of one vault (or all) and compact its log.
    Snapshot {
        /// `user/vault`; omit for every vault.
        target: Option<String>,
        /// Snapshots to keep per vault after pruning.
        #[arg(long, default_value_t = 14)]
        keep: usize,
    },
    /// Create a tar.gz backup of the data directory (safe while the server runs).
    Backup {
        /// Output file, or `-` for stdout (pipe into age/gpg to encrypt).
        #[arg(long, short)]
        out: Option<String>,
        /// Only this user's vaults.
        #[arg(long)]
        user: Option<String>,
        /// Only this vault name (with --user).
        #[arg(long)]
        vault: Option<String>,
    },
    /// Restore a tar.gz backup into the data directory. Stop the server first.
    Restore {
        /// Input file, or `-` for stdin.
        #[arg(long, short)]
        from: String,
        /// Allow restoring into a non-empty data dir (adds/overwrites the vaults in the archive).
        #[arg(long)]
        merge: bool,
    },
    /// Roll one vault back to a snapshot file. Destructive; stop the server first.
    RestoreSnapshot {
        /// `user/vault`
        target: String,
        /// Path to a `.ybin` snapshot.
        file: PathBuf,
        /// Confirm you understand the current log is replaced.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Parser, Debug, Clone)]
#[command(name = "serve")]
pub struct ServeArgs {
    /// Address to listen on.
    #[arg(long, env = "SAMSYN_BIND", default_value = "0.0.0.0:8787")]
    pub bind: SocketAddr,

    /// Maximum blob size in megabytes.
    #[arg(long, env = "SAMSYN_MAX_BLOB_MB", default_value_t = 256)]
    pub max_blob_mb: usize,

    /// Hours between automatic snapshots of active vaults (0 disables).
    #[arg(long, env = "SAMSYN_SNAPSHOT_HOURS", default_value_t = 24)]
    pub snapshot_hours: u64,

    /// Automatic snapshots to keep per vault.
    #[arg(long, env = "SAMSYN_SNAPSHOT_KEEP", default_value_t = 14)]
    pub snapshot_keep: usize,

    /// PEM certificate chain. With --tls-key, serve HTTPS/WSS directly (required for iOS).
    #[arg(long, env = "SAMSYN_TLS_CERT", requires = "tls_key")]
    pub tls_cert: Option<PathBuf>,

    /// PEM private key for --tls-cert.
    #[arg(long, env = "SAMSYN_TLS_KEY", requires = "tls_cert")]
    pub tls_key: Option<PathBuf>,

    /// Relay server URL(s) for server-to-server sharing (repeat or comma-separate).
    /// Default: the public n0 relays. "none" disables relays (direct connections only).
    #[arg(long, env = "SAMSYN_RELAY", value_delimiter = ',')]
    pub relay: Vec<String>,

    /// UDP port for the peer-to-peer endpoint (0 = random). A fixed port keeps the direct
    /// addresses in old tickets valid across restarts; forwarding it is optional (relays
    /// cover the rest).
    #[arg(long, env = "SAMSYN_P2P_PORT", default_value_t = 8788)]
    pub p2p_port: u16,

    /// Public HTTPS URL of this server, embedded in tickets so people without a server of
    /// their own can join a share directly from the plugin ("guest mode"). Unset = guests off.
    #[arg(long, env = "SAMSYN_PUBLIC_URL")]
    pub public_url: Option<String>,

    /// Upload quota per share for guests, in megabytes (0 = unlimited). Guests of a share may
    /// not push blobs once the share's blob store is this large.
    #[arg(long, env = "SAMSYN_MAX_SHARE_MB", default_value_t = 2048)]
    pub max_share_mb: u64,

    /// Largest document (vault or share) in megabytes of CRDT state; updates past it are
    /// refused (0 = unlimited). Text only: attachments live in blobs.
    #[arg(long, env = "SAMSYN_MAX_DOC_MB", default_value_t = 256)]
    pub max_doc_mb: u64,

    /// Concurrent WebSocket connections per user token (0 = unlimited).
    #[arg(long, env = "SAMSYN_MAX_CONNS_PER_TOKEN", default_value_t = 16)]
    pub max_conns_per_token: usize,

    /// Concurrent guest connections per ticket (0 = unlimited).
    #[arg(long, env = "SAMSYN_MAX_CONNS_PER_TICKET", default_value_t = 8)]
    pub max_conns_per_ticket: usize,

    /// Concurrent p2p connections per peer server (0 = unlimited).
    #[arg(long, env = "SAMSYN_MAX_CONNS_PER_PEER", default_value_t = 4)]
    pub max_conns_per_peer: usize,

    /// Unload a vault from memory after this many minutes without connections or shares
    /// (0 = never). It is reloaded from disk on the next connection.
    #[arg(long, env = "SAMSYN_IDLE_UNLOAD_MINUTES", default_value_t = 30)]
    pub idle_unload_minutes: u64,

    /// Extra allowed CORS origins (repeat or comma-separate). Obsidian's own origins are
    /// always allowed; the plugin does not need CORS at all (it uses native requests).
    #[arg(long, env = "SAMSYN_CORS_ORIGIN", value_delimiter = ',')]
    pub cors_origin: Vec<String>,

    /// Allow any CORS origin (the old behaviour). Harmless with bearer tokens, but off by default.
    #[arg(long, env = "SAMSYN_CORS_ANY", default_value_t = false)]
    pub cors_any: bool,

    /// On first start, write the owner token to this file (mode 0600) instead of the log.
    #[arg(long, env = "SAMSYN_INITIAL_TOKEN_FILE")]
    pub initial_token_file: Option<PathBuf>,

    /// Storage per user in megabytes (blobs plus document logs over all their vaults and
    /// shares; 0 = unlimited). Uploads past it are refused with 413.
    #[arg(long, env = "SAMSYN_MAX_USER_MB", default_value_t = 20480)]
    pub max_user_mb: u64,

    /// Vaults one user may create (0 = unlimited). A vault is created on first use of a name.
    #[arg(long, env = "SAMSYN_MAX_VAULTS_PER_USER", default_value_t = 50)]
    pub max_vaults_per_user: usize,

    /// Behind a reverse proxy: take the client address from the last X-Forwarded-For entry
    /// (for guest rate limiting). Never set this when clients can reach the server directly.
    #[arg(long, env = "SAMSYN_TRUST_PROXY", default_value_t = false)]
    pub trust_proxy: bool,

    /// Disable the peer-to-peer endpoint entirely (tickets then only work on this server).
    #[arg(long, env = "SAMSYN_NO_P2P", default_value_t = false)]
    pub no_p2p: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum UserCmd {
    /// Create a user and print their first token.
    Add { name: String },
    /// List users.
    List,
    /// Issue an additional token for a user (e.g. one per device).
    Token {
        name: String,
        #[arg(long, default_value = "device")]
        label: String,
    },
    /// Revoke tokens of a user (all, or one label).
    Revoke {
        name: String,
        #[arg(long)]
        label: Option<String>,
    },
    /// Delete a user from the registry. Their vault directories are left on disk.
    Remove { name: String },
}

#[derive(Subcommand, Debug, Clone)]
pub enum VaultCmd {
    /// List vaults, optionally for one user.
    List {
        #[arg(long)]
        user: Option<String>,
    },
}

/// Extract a bearer token from the `Authorization` header or the `token` query param.
/// Obsidian mobile cannot set WebSocket headers, hence the query param fallback.
pub fn extract_token(headers: &axum::http::HeaderMap, query_token: Option<&str>) -> Option<String> {
    if let Some(v) = headers.get(axum::http::header::AUTHORIZATION) {
        if let Ok(s) = v.to_str() {
            if let Some(rest) = s.strip_prefix("Bearer ") {
                return Some(rest.trim().to_string());
            }
        }
    }
    query_token.map(|s| s.to_string())
}
