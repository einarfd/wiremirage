//! clap argument tree.
//!
//! Mirrors the surface in [[cli-design.md]] minus the items deferred
//! for this slice (no `--from-file`, no `wm match`, no `wm journal
//! tail`, no admin user CRUD). The shape is conservative and
//! verb-after-noun — agents read it via `wm --help` so the structure
//! has to be predictable.

use clap::{Parser, Subcommand};

const DEFAULT_HOST: &str = "http://localhost:8080";

#[derive(Debug, Parser)]
#[command(name = "wm", version, about = "WireMirage CLI", long_about = None)]
pub struct Cli {
    /// Host URL. Defaults to `http://localhost:8080`. Read from the
    /// `WM_HOST` env var if not supplied on the command line.
    #[arg(long, env = "WM_HOST", default_value = DEFAULT_HOST, global = true)]
    pub host: String,

    /// Bearer token (`wmt_...`). Required for everything under
    /// `/__api/*`. Read from the `WM_TOKEN` env var if not supplied
    /// on the command line. `wm health` and `wm version` work without
    /// a token.
    #[arg(long, env = "WM_TOKEN", global = true)]
    pub token: Option<String>,

    /// Emit machine-parseable JSON instead of human-readable text.
    /// Off by default; the human format is the contract for
    /// interactive use, the JSON shape is the contract for scripts.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Probe `/__health`.
    Health,
    /// Print CLI and (when reachable) host version info.
    Version,
    /// Manage groups (lifecycle units for routes).
    #[command(subcommand)]
    Groups(GroupsCommand),
    /// Manage routes (the individual mocks).
    #[command(subcommand)]
    Routes(RoutesCommand),
    /// Inspect request journal entries.
    #[command(subcommand)]
    Journal(JournalCommand),
    /// Manage API tokens.
    #[command(subcommand)]
    Tokens(TokensCommand),
}

#[derive(Debug, Subcommand)]
pub enum GroupsCommand {
    /// List groups. Non-admin sees only their own; admin sees all.
    List,
    /// Create a group.
    Create(CreateGroupArgs),
    /// Show one group.
    Show {
        /// Group name or ULID.
        name: String,
    },
    /// Update a group's mutable fields (TTL, sliding-TTL).
    Update(UpdateGroupArgs),
    /// Delete a group. Cascades routes, kv state, and journal entries.
    Delete {
        /// Group name or ULID.
        name: String,
        /// Skip confirmation. Required in non-interactive mode for
        /// scripts; we don't actually prompt today, but the flag
        /// reserves the verb shape.
        #[arg(long)]
        force: bool,
    },
    /// Bump the group's expiry forward by its configured TTL.
    Refresh {
        /// Group name or ULID.
        name: String,
    },
    /// Manage the group's per-route and per-group kv state.
    State {
        /// Group name or ULID.
        name: String,
        /// Clear all kv: and gkv: state for the group. Routes
        /// themselves stay alive.
        #[arg(long)]
        clear: bool,
    },
    /// Clear journal entries for the group. (`wm journal list <group>`
    /// is the read counterpart; the per-group clear lives here so
    /// it's discoverable via `wm groups --help`.)
    Journal {
        /// Group name or ULID.
        name: String,
        /// Clear all journal entries for this group. Routes and kv
        /// state stay alive.
        #[arg(long)]
        clear: bool,
    },
}

#[derive(Debug, clap::Args)]
pub struct CreateGroupArgs {
    /// Group name (canonical external identifier).
    pub name: String,
    /// TTL in seconds. Default is the host's `default_group_ttl`
    /// (24h). Must not exceed the configured maximum (30d).
    #[arg(long)]
    pub ttl_seconds: Option<u64>,
    /// Disable sliding TTL — the group expires after `ttl_seconds`
    /// from creation regardless of activity. Sliding is the default;
    /// pass this flag for fixed-window expiry.
    #[arg(long, conflicts_with = "sliding")]
    pub no_sliding: bool,
    /// Explicitly request sliding TTL. (No-op since sliding is the
    /// default; included for symmetry and so `--sliding` is a valid
    /// thing to type.)
    #[arg(long)]
    pub sliding: bool,
}

#[derive(Debug, clap::Args)]
pub struct UpdateGroupArgs {
    /// Group name or ULID.
    pub name: String,
    /// New TTL in seconds. The Valkey TTL is reset to this on update.
    #[arg(long)]
    pub ttl_seconds: Option<u64>,
    /// Enable sliding TTL.
    #[arg(long, conflicts_with = "no_sliding")]
    pub sliding: bool,
    /// Disable sliding TTL.
    #[arg(long)]
    pub no_sliding: bool,
}

#[derive(Debug, Subcommand)]
pub enum RoutesCommand {
    /// List routes.
    List,
    /// Add a route. Pass exactly one of `--source-file` (handler
    /// source for compile via the sidecar) or `--wasm-file`
    /// (pre-built `.component.wasm`).
    Add(AddRouteArgs),
    /// Show one route.
    Show {
        /// Route slug `{group}/{n}` (e.g. `stripe-mock/7`).
        slug: String,
    },
    /// Delete a route.
    Delete {
        /// Route slug `{group}/{n}`.
        slug: String,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, clap::Args)]
pub struct AddRouteArgs {
    /// Group to add the route to (name or ULID). Optional — omit
    /// to land in an implicit single-route group named `_route_*`.
    #[arg(long)]
    pub group: Option<String>,
    /// HTTP method or comma-separated list. `ANY` matches everything.
    #[arg(long)]
    pub method: String,
    /// Path pattern. May contain `{param}` segments.
    #[arg(long)]
    pub path: String,
    /// Read handler source from this file. The file's contents are
    /// shipped to the host as the `source` field; the host forwards
    /// to the compiler sidecar.
    #[arg(long, conflicts_with = "wasm_file")]
    pub source_file: Option<String>,
    /// Read a pre-built component from this file. The file's bytes
    /// are base64-encoded and shipped as `compiled_wasm` with
    /// `language: "wasm"`.
    #[arg(long)]
    pub wasm_file: Option<String>,
    /// Source language. Required (and validated host-side) when
    /// using `--source-file`. Defaults to `typescript`. Ignored when
    /// using `--wasm-file`.
    #[arg(long, default_value = "typescript")]
    pub language: String,
    /// `bindings_version` declared on the upload. Required for
    /// `--wasm-file`; ignored for `--source-file` (the compiler
    /// sets it).
    #[arg(long, default_value = "0.1.0")]
    pub bindings_version: String,
}

#[derive(Debug, Subcommand)]
pub enum JournalCommand {
    /// List journal entries for a group, newest first.
    List {
        /// Group name or ULID.
        group: String,
        /// Cursor for the next page: return entries with `number <
        /// before`. Omit to start at the newest.
        #[arg(long)]
        before: Option<u32>,
        /// Max entries per page. Capped at 100 host-side.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Show one journal entry.
    Show {
        /// Entry slug — `{group}/journal/{n}` or `{group}/{n}`.
        slug: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum TokensCommand {
    /// List the caller's tokens (plaintext is never returned here).
    List,
    /// Create a new token. The plaintext appears once in the
    /// response — save it then.
    Create(CreateTokenArgs),
    /// Revoke a token by name.
    Revoke {
        /// Token name.
        name: String,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, clap::Args)]
pub struct CreateTokenArgs {
    /// Token name (unique per owner).
    pub name: String,
    /// Optional TTL in seconds. Tokens default to no expiry.
    #[arg(long)]
    pub ttl_seconds: Option<u64>,
}
