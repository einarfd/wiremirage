//! clap argument tree.
//!
//! Mirrors the surface in [[cli-design.md]] minus the items deferred
//! for this slice (no `--from-file`, no `wm match`, no `wm journal
//! tail`, no admin user CRUD). The shape is conservative and
//! verb-after-noun — agents read it via `wm --help` so the structure
//! has to be predictable.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "wm", version, about = "WireMirage CLI", long_about = None)]
pub struct Cli {
    /// Host URL. Read from `WM_HOST` if not supplied; falls back to
    /// the selected profile's `host` field, then to
    /// `http://localhost:8080`. See `cli-design.md` for the full
    /// resolution order.
    #[arg(long, env = "WM_HOST", global = true)]
    pub host: Option<String>,

    /// Bearer token (`wmt_...`). Required for everything under
    /// `/api/*`. Read from `WM_TOKEN` if not supplied; falls back
    /// to the selected profile's `token` field. `wm health` and
    /// `wm version` work without a token.
    #[arg(long, env = "WM_TOKEN", global = true)]
    pub token: Option<String>,

    /// Configuration profile to use. Read from `WM_PROFILE` if not
    /// supplied; falls back to `default`. Profiles live in
    /// `~/.config/wiremirage/config.toml` (override with
    /// `WM_CONFIG_FILE`) under `[profiles.NAME]` keys.
    #[arg(long, env = "WM_PROFILE", global = true)]
    pub profile: Option<String>,

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
    /// Probe `/health`.
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
    /// Inspect outbound-callback delivery outcomes for a group.
    #[command(subcommand)]
    Callbacks(CallbacksCommand),
    /// Inspect unmatched-request entries for groups you own (an admin
    /// sees host-wide).
    #[command(subcommand)]
    Unmatched(UnmatchedCommand),
    /// Manage API tokens.
    #[command(subcommand)]
    Tokens(TokensCommand),
    /// Manage users (admin-only for cross-user actions).
    #[command(subcommand)]
    Users(UsersCommand),
    /// Probe what would match a hypothetical request, within a group.
    /// Matching is per-subdomain, so a group is required.
    Match {
        /// Group (name or ULID) to probe within.
        #[arg(short, long)]
        group: String,
        /// HTTP method (e.g. `GET`, `POST`, `ANY`).
        method: String,
        /// Request path (must start with `/`).
        path: String,
    },
    /// Print handler-API documentation (same content as the MCP
    /// `get_capabilities` tool). Call without `topic` for the
    /// overview + topic list. Known topics: `overview`, `types`,
    /// `request`, `response`, `store`, `log`, `clock`, `streaming`,
    /// `callbacks`, `gotchas`. `types` prints the handler `.d.ts` —
    /// `wm capabilities types > wiremirage-handler.d.ts`.
    Capabilities {
        /// Topic name. Omit for the overview. Unknown topics fall
        /// back to the overview.
        topic: Option<String>,
    },
    /// Generate a shell completion script. Pipe into the appropriate
    /// location for your shell (e.g. `wm completion bash >
    /// /etc/bash_completion.d/wm`). No host or token required.
    Completion {
        /// Target shell.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[derive(Debug, Subcommand)]
pub enum GroupsCommand {
    /// List groups. Non-admin sees only their own; admin sees all.
    List(GroupsListArgs),
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
    /// Manage the group's shared (`gkv:`) and per-route (`kv:`) state.
    /// Default lists the shared store; `--set` upserts into it,
    /// `--snapshot` dumps it as round-trippable JSON, `--reset-from`
    /// resets it to a baseline, `--clear` wipes all group state.
    State {
        /// Group name or ULID.
        name: String,
        /// Clear all kv: and gkv: state for the group. Routes
        /// themselves stay alive.
        #[arg(long)]
        clear: bool,
        /// Upsert a key into the group's shared store (repeatable):
        /// `--set KEY=VALUE` (UTF-8 value).
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// Print a round-trippable snapshot of the shared store as JSON.
        #[arg(long)]
        snapshot: bool,
        /// Clear, then seed the shared store from a snapshot JSON file.
        #[arg(long = "reset-from", value_name = "FILE")]
        reset_from: Option<String>,
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
    /// Export a group as a YAML or JSON spec file. Round-trips with
    /// `wm groups create --from-file`. Wasm-uploaded routes are
    /// rejected — only source-language routes can be represented
    /// in the spec format.
    Export(ExportGroupArgs),
}

#[derive(Debug, clap::Args)]
pub struct CreateGroupArgs {
    /// Group name (canonical external identifier; also the group's
    /// subdomain, so it must be a valid DNS label). Optional — omit to be
    /// assigned a friendly name automatically. Also omit when using
    /// `--from-file` (the name then comes from the spec file).
    pub name: Option<String>,
    /// Create the group and every route it contains from a YAML/JSON
    /// spec file. Format is detected from the extension (`.yaml`,
    /// `.yml`, `.json`); pass `-` to read from stdin and use
    /// `--format` to disambiguate. On any failure, partial state is
    /// rolled back — either the whole group lands or nothing does.
    #[arg(long, value_name = "FILE")]
    pub from_file: Option<String>,
    /// Spec format for `--from-file -` (stdin). Ignored when
    /// `--from-file` points at a real file.
    #[arg(long, value_enum, default_value = "yaml")]
    pub format: SpecFormatArg,
    /// TTL in seconds. Default is the host's `default_group_ttl`
    /// (24h). Must not exceed the configured maximum (30d).
    /// Ignored when `--from-file` is supplied — the spec's `ttl`
    /// field wins there.
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
pub struct ExportGroupArgs {
    /// Group name or ULID.
    pub name: String,
    /// Output format. Default `yaml`.
    #[arg(long, value_enum, default_value = "yaml")]
    pub format: SpecFormatArg,
    /// Write the rendered spec to FILE instead of stdout.
    #[arg(long, value_name = "FILE")]
    pub output: Option<String>,
}

/// CLI-facing alias for `spec::SpecFormat` so we can derive
/// `ValueEnum` without polluting the cross-format type.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum SpecFormatArg {
    Yaml,
    Json,
}

impl From<SpecFormatArg> for crate::spec::SpecFormat {
    fn from(a: SpecFormatArg) -> Self {
        match a {
            SpecFormatArg::Yaml => crate::spec::SpecFormat::Yaml,
            SpecFormatArg::Json => crate::spec::SpecFormat::Json,
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct UpdateGroupArgs {
    /// Group name or ULID.
    pub name: String,
    /// Rename the group to this name. It's also the group's subdomain, so it
    /// must be a valid DNS label — and renaming changes the served base URL.
    #[arg(long)]
    pub rename: Option<String>,
    /// New TTL in seconds. The Valkey TTL is reset to this on update.
    #[arg(long)]
    pub ttl_seconds: Option<u64>,
    /// Enable sliding TTL.
    #[arg(long, conflicts_with = "no_sliding")]
    pub sliding: bool,
    /// Disable sliding TTL.
    #[arg(long)]
    pub no_sliding: bool,
    /// Allow this group's handlers to make outbound callbacks.
    #[arg(long, conflicts_with = "no_callout")]
    pub callout: bool,
    /// Disallow outbound callbacks for this group (the default).
    #[arg(long)]
    pub no_callout: bool,
}

#[derive(Debug, Subcommand)]
pub enum RoutesCommand {
    /// List routes.
    List(RoutesListArgs),
    /// Add a route. Pass `--source-file` (handler source, compiled
    /// in-process) and optionally `--language`.
    Add(AddRouteArgs),
    /// Show one route.
    Show {
        /// Route slug `{group}/{n}` (e.g. `stripe-mock/7`).
        slug: String,
    },
    /// Update a route's mutable fields. Pass at least one of `--method`,
    /// `--path`, or `--source-file`. Owner-or-admin only.
    Update(UpdateRouteArgs),
    /// Inspect or write the route's per-route kv state. Default lists;
    /// `--set` upserts, `--snapshot` dumps a round-trippable JSON,
    /// `--reset-from` resets to a baseline, `--clear` wipes. Owner-or-
    /// admin only.
    State {
        /// Route slug `{group}/{n}`.
        slug: String,
        /// Wipe all per-route kv keys for this route. The route
        /// itself stays alive.
        #[arg(long)]
        clear: bool,
        /// Upsert a key (repeatable): `--set KEY=VALUE` (UTF-8 value).
        /// Listed keys are written; others left untouched.
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// Print a round-trippable snapshot (full values) as JSON
        /// instead of the key listing.
        #[arg(long)]
        snapshot: bool,
        /// Clear, then seed from a snapshot JSON file
        /// (`{"entries":{...}}`) — a reset to a known baseline.
        #[arg(long = "reset-from", value_name = "FILE")]
        reset_from: Option<String>,
    },
    /// Print the original handler source the route was created from.
    /// Empty (with a note) for pre-compiled `wasm` uploads. Owner-or-
    /// admin only.
    Source {
        /// Route slug `{group}/{n}`.
        slug: String,
    },
    /// Dry-run the route's handler against a synthetic request. State
    /// reads see a point-in-time snapshot; writes land in the
    /// snapshot and are discarded after the call. Skips the journal.
    Test(TestRouteArgs),
    /// Delete a route.
    Delete {
        /// Route slug `{group}/{n}`.
        slug: String,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, clap::Args)]
pub struct TestRouteArgs {
    /// Route slug `{group}/{n}` (e.g. `stripe-mock/7`).
    pub slug: String,
    /// HTTP method to pass to the handler.
    #[arg(long, default_value = "GET")]
    pub method: String,
    /// Request path. Defaults to the route's own path. Must start
    /// with `/` if supplied.
    #[arg(long)]
    pub path: Option<String>,
    /// `KEY: VALUE` header. Repeatable.
    #[arg(long = "header", value_name = "KEY:VALUE")]
    pub headers: Vec<String>,
    /// Inline request body. Use `@FILE` to read from disk.
    #[arg(long)]
    pub body: Option<String>,
    /// `name=value` path-param override. Repeatable. By default the
    /// handler sees an empty path-params list (the dry-run path
    /// doesn't re-run the matcher).
    #[arg(long = "path-param", value_name = "NAME=VALUE")]
    pub path_params: Vec<String>,
    /// `key=value` entry to seed into the route's private `kv:`
    /// snapshot before the handler runs. Repeatable. Lets you test
    /// state-dependent branches (`if counter > 3`) without first
    /// driving real traffic. Value is sent as UTF-8 bytes; real `kv:`
    /// state is never touched.
    #[arg(long = "kv", value_name = "KEY=VALUE")]
    pub kv_overrides: Vec<String>,
    /// Same as `--kv`, scoped to the group's shared `gkv:`.
    #[arg(long = "gkv", value_name = "KEY=VALUE")]
    pub gkv_overrides: Vec<String>,
}

#[derive(Debug, clap::Args)]
pub struct AddRouteArgs {
    /// Group to add the route to (name or ULID). Optional — omit
    /// to land in an auto-named implicit single-route group.
    #[arg(long)]
    pub group: Option<String>,
    /// HTTP method or comma-separated list. `ANY` matches everything.
    #[arg(long)]
    pub method: String,
    /// Path pattern. May contain `{param}` segments.
    #[arg(long)]
    pub path: String,
    /// Read handler source from this file. The contents are shipped
    /// to the host as the `source` field; the host compiles in-process
    /// (TS via swc, JS verbatim). This is the only artifact input.
    #[arg(long)]
    pub source_file: Option<String>,
    /// Source language. Defaults to `typescript`; pass `javascript`
    /// for plain JS. Validated host-side.
    #[arg(long, default_value = "typescript")]
    pub language: String,
}

#[derive(Debug, clap::Args)]
pub struct UpdateRouteArgs {
    /// Route slug `{group}/{n}` (e.g. `stripe-mock/7`).
    pub slug: String,
    /// Replace the method list. Comma-separated; `ANY` matches every
    /// method. Omit to leave the existing list alone.
    #[arg(long)]
    pub method: Option<String>,
    /// Replace the path pattern. Omit to leave the existing path
    /// alone.
    #[arg(long)]
    pub path: Option<String>,
    /// Replace the handler with source from this file. Compiled
    /// in-process (TS via swc, JS verbatim).
    #[arg(long)]
    pub source_file: Option<String>,
    /// Source language for `--source-file`. Defaults to `typescript`.
    #[arg(long, default_value = "typescript")]
    pub language: String,
}

#[derive(Debug, clap::Args)]
pub struct GroupsListArgs {
    /// Filter to groups owned by this user (name or ULID). Admin-only;
    /// non-admin callers automatically see only their own groups.
    #[arg(long)]
    pub owner_id: Option<String>,
    /// Match groups whose name starts with this prefix.
    #[arg(long)]
    pub name_prefix: Option<String>,
    /// Free-text needle (case-insensitive substring match against name).
    #[arg(long)]
    pub q: Option<String>,
    /// Lower bound on `last_activity_at`. Duration suffix (`5m`, `1h`,
    /// `2d`, `30s`) or RFC 3339 timestamp.
    #[arg(long)]
    pub since: Option<String>,
    /// Upper bound on `last_activity_at`. Same format as `--since`.
    #[arg(long)]
    pub until: Option<String>,
    /// Show only implicit groups (`true`) or only explicit (`false`).
    /// Omit for both.
    #[arg(long)]
    pub implicit: Option<bool>,
    /// Sort column: `created_at` (default), `name`, `last_activity_at`.
    #[arg(long)]
    pub sort: Option<String>,
    /// Sort direction: `asc` or `desc`. Default `desc`.
    #[arg(long)]
    pub dir: Option<String>,
    /// Start offset for pagination. Default 0.
    #[arg(long)]
    pub offset: Option<u64>,
    /// Page size. Default 50, max 200.
    #[arg(long)]
    pub limit: Option<u64>,
}

#[derive(Debug, clap::Args)]
pub struct RoutesListArgs {
    /// Filter to routes inside this group (name or ULID).
    #[arg(long)]
    pub group: Option<String>,
    /// Filter to routes owned by this user. Admin-only.
    #[arg(long)]
    pub owner_id: Option<String>,
    /// HTTP method filter (uppercase, e.g. `GET`, or `ANY`).
    #[arg(long)]
    pub method: Option<String>,
    /// `*`-glob over the route's path (e.g. `/v1/*`).
    #[arg(long)]
    pub path_pattern: Option<String>,
    /// Lower bound on `last_hit_at`. Duration or RFC 3339.
    #[arg(long)]
    pub since: Option<String>,
    /// Upper bound on `last_hit_at`. Same format as `--since`.
    #[arg(long)]
    pub until: Option<String>,
    /// Free-text needle (case-insensitive substring match against
    /// path or methods).
    #[arg(long)]
    pub q: Option<String>,
    /// Sort column: `created_at` (default), `last_hit_at`, `hits_total`.
    #[arg(long)]
    pub sort: Option<String>,
    /// Sort direction. Default `desc`.
    #[arg(long)]
    pub dir: Option<String>,
    #[arg(long)]
    pub offset: Option<u64>,
    #[arg(long)]
    pub limit: Option<u64>,
}

#[derive(Debug, Subcommand)]
pub enum JournalCommand {
    /// List journal entries for a group, newest first.
    List(JournalListArgs),
    /// Show one journal entry.
    Show {
        /// Entry slug — `{group}/journal/{n}` or `{group}/{n}`.
        slug: String,
    },
}

#[derive(Debug, clap::Args)]
pub struct JournalListArgs {
    /// Group name or ULID.
    pub group: String,
    /// Cursor for the next page: return entries with `number <
    /// before`. Omit to start at the newest.
    #[arg(long)]
    pub before: Option<u32>,
    /// Max entries per page. Capped at 100 host-side.
    #[arg(long)]
    pub limit: Option<usize>,
    /// Restrict to a single route within the group. Slug form
    /// `{group}/{n}` (the path-scoped group must match).
    #[arg(long)]
    pub route: Option<String>,
    /// HTTP method filter.
    #[arg(long)]
    pub method: Option<String>,
    /// `*`-glob over the entry's `matched_pattern`.
    #[arg(long)]
    pub path_pattern: Option<String>,
    /// Status filter: `2xx` / `3xx` / `4xx` / `5xx` or an exact code
    /// like `503`.
    #[arg(long)]
    pub status: Option<String>,
    /// Lower bound on `created_at`. Duration or RFC 3339.
    #[arg(long)]
    pub since: Option<String>,
    /// Upper bound on `created_at`. Same format as `--since`.
    #[arg(long)]
    pub until: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum CallbacksCommand {
    /// List a group's outbound-callback outcomes, newest first.
    List(CallbacksListArgs),
    /// Show one callback record by its number.
    Show {
        /// Group name or ULID.
        group: String,
        /// Callback number (from `list`).
        number: u32,
    },
}

#[derive(Debug, clap::Args)]
pub struct CallbacksListArgs {
    /// Group name or ULID.
    pub group: String,
    /// Cursor for the next page: return entries with `number < before`.
    #[arg(long)]
    pub before: Option<u32>,
    /// Max entries per page. Capped at 100 host-side.
    #[arg(long)]
    pub limit: Option<usize>,
}

#[derive(Debug, Subcommand)]
pub enum UnmatchedCommand {
    /// List unmatched-request entries for groups you own (an admin sees
    /// host-wide).
    List(UnmatchedListArgs),
    /// Show one unmatched entry by its journal number.
    Show {
        /// Entry number (host-wide, monotonic).
        number: u64,
    },
}

#[derive(Debug, clap::Args)]
pub struct UnmatchedListArgs {
    /// Cursor for the next page: return entries with `number <
    /// before`. Omit to start at the newest.
    #[arg(long)]
    pub before: Option<u64>,
    /// Max entries per page. Capped at 100 host-side.
    #[arg(long)]
    pub limit: Option<usize>,
    /// HTTP method filter.
    #[arg(long)]
    pub method: Option<String>,
    /// `*`-glob over the request path.
    #[arg(long)]
    pub path_pattern: Option<String>,
    /// Lower bound on `created_at`.
    #[arg(long)]
    pub since: Option<String>,
    /// Upper bound on `created_at`.
    #[arg(long)]
    pub until: Option<String>,
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
    /// Rename a token. The secret value is unchanged — only the name.
    Rename {
        /// Current token name.
        name: String,
        /// New token name (unique per owner).
        new_name: String,
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

#[derive(Debug, Subcommand)]
pub enum UsersCommand {
    /// List users. Admin-only.
    List,
    /// Show one user by email. Admin sees any user; non-admin sees
    /// their own record only (the host enforces).
    Show {
        /// The user's email (the account identifier).
        email: String,
    },
    /// Show the authenticated user's own record. Always available.
    Me,
    /// Create a new user. Admin-only.
    Create(CreateUserArgs),
    /// Update a user's admin flag. Admin-only.
    Update(UpdateUserArgs),
    /// Delete a user. Admin-only. The host refuses to delete the
    /// last admin or a user that owns routes.
    Delete {
        /// The user's email (the account identifier).
        email: String,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, clap::Args)]
pub struct CreateUserArgs {
    /// The user's email (the account identifier; unique). Users who
    /// log in via OAuth/OIDC are usually provisioned by that login
    /// instead — a pre-created email links to their first login.
    pub email: String,
    /// Create the user as an admin. Default is non-admin.
    #[arg(long)]
    pub admin: bool,
}

#[derive(Debug, clap::Args)]
pub struct UpdateUserArgs {
    /// The user's email (the account identifier).
    pub email: String,
    /// Promote the user to admin.
    #[arg(long, conflicts_with = "no_admin")]
    pub admin: bool,
    /// Demote the user from admin.
    #[arg(long)]
    pub no_admin: bool,
}
