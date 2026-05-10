//! Subcommand dispatch. Each handler maps clap args + `wm_core::Client`
//! to one or more REST calls, then formats the response. Exit codes
//! follow the conventions in [[cli-design.md]]:
//!
//! - 0  — success
//! - 1  — generic error (network failure, server 5xx, etc.)
//! - 2  — usage (clap handles this before reaching `dispatch`)
//! - 4  — auth error (no token, 401, 403)
//! - 5  — not found (404)
//! - 6  — conflict (409)

use std::path::Path;
use std::process::ExitCode;

use anyhow::Context;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use wm_core::{
    Client, ClientError, CreateGroupBody, CreateRouteBody, CreateTokenBody, PatchGroupBody,
};

use crate::cli::{
    AddRouteArgs, Cli, Command, CreateTokenArgs, GroupsCommand, JournalCommand, RoutesCommand,
    TokensCommand,
};
use crate::format::{self, Format};

/// Top-level dispatcher. Returns an `ExitCode` so the binary's `main`
/// can propagate. `Err(anyhow::Error)` is reserved for truly
/// unexpected client-side problems (failed to read a file, etc.) —
/// HTTP-level errors return `Ok(ExitCode)` after printing the
/// per-class message.
pub async fn dispatch(args: Cli) -> anyhow::Result<ExitCode> {
    let format = Format::from_flag(args.json);

    // Two commands work without a token. Fast-path them so missing
    // token doesn't bother the user. (`wm match` does need a token —
    // it's a host-side probe, not a local computation.)
    let needs_token = !matches!(args.command, Command::Health | Command::Version);
    if needs_token && args.token.is_none() {
        emit_error(
            format,
            "auth",
            "no token configured. Set WM_TOKEN or pass --token wmt_...",
        );
        return Ok(ExitCode::from(4));
    }

    let mut builder = Client::builder(&args.host);
    if let Some(token) = args.token.as_deref() {
        builder = builder.with_token(token);
    }
    let client = builder.build().context("build wm-core client")?;

    let result = match args.command {
        Command::Health => handle_health(&client, format).await,
        Command::Version => handle_version(&client, format).await,
        Command::Groups(cmd) => handle_groups(&client, cmd, format).await,
        Command::Routes(cmd) => handle_routes(&client, cmd, format).await,
        Command::Journal(cmd) => handle_journal(&client, cmd, format).await,
        Command::Tokens(cmd) => handle_tokens(&client, cmd, format).await,
        Command::Match { method, path } => handle_match(&client, &method, &path, format).await,
    };

    match result {
        Ok(()) => Ok(ExitCode::from(0)),
        Err(e) => {
            let (code, label) = exit_code_for(&e);
            emit_error(format, label, &client_error_message(&e));
            Ok(code)
        }
    }
}

fn exit_code_for(err: &ClientError) -> (ExitCode, &'static str) {
    match err {
        ClientError::Unauthorized(_) | ClientError::Forbidden(_) => (ExitCode::from(4), "auth"),
        ClientError::NotFound(_) => (ExitCode::from(5), "not_found"),
        ClientError::Conflict(_) => (ExitCode::from(6), "conflict"),
        _ => (ExitCode::from(1), "error"),
    }
}

fn client_error_message(err: &ClientError) -> String {
    match err {
        ClientError::Unauthorized(m)
        | ClientError::Forbidden(m)
        | ClientError::NotFound(m)
        | ClientError::Conflict(m)
        | ClientError::Validation(m)
        | ClientError::BadResponse(m)
        | ClientError::Network(m)
        | ClientError::InvalidHost(m) => m.clone(),
        ClientError::ServerError { status, message } => {
            format!("server returned {status}: {message}")
        }
    }
}

fn emit_error(format: Format, code: &str, message: &str) {
    match format {
        Format::Json => {
            let body = serde_json::json!({
                "error": { "code": code, "message": message }
            });
            eprintln!("{}", serde_json::to_string(&body).unwrap_or_default());
        }
        Format::Human => {
            eprintln!("error ({code}): {message}");
        }
    }
}

// -- Health & version --------------------------------------------------------

async fn handle_health(client: &Client, format: Format) -> Result<(), ClientError> {
    let h = client.health().await?;
    format::render_health(&h, format);
    Ok(())
}

async fn handle_version(client: &Client, format: Format) -> Result<(), ClientError> {
    // CLI version is the build's own; host version comes from
    // /__health when reachable. Best-effort: a network failure
    // shouldn't block printing the CLI version.
    let cli_version = env!("CARGO_PKG_VERSION");
    let host_version = client.health().await.ok().map(|h| h.version);
    match format {
        Format::Json => format::print_json(&serde_json::json!({
            "cli_version": cli_version,
            "host_version": host_version,
        })),
        Format::Human => {
            println!("wm-cli:  {cli_version}");
            match host_version {
                Some(v) => println!("wm-host: {v}"),
                None => println!("wm-host: (unreachable)"),
            }
        }
    }
    Ok(())
}

// -- Groups ------------------------------------------------------------------

async fn handle_groups(
    client: &Client,
    cmd: GroupsCommand,
    format: Format,
) -> Result<(), ClientError> {
    match cmd {
        GroupsCommand::List => {
            let list = client.list_groups().await?;
            format::render_group_list(&list, format);
        }
        GroupsCommand::Create(args) => {
            let body = CreateGroupBody {
                name: args.name,
                ttl_seconds: args.ttl_seconds,
                sliding_ttl: sliding_flag(args.sliding, args.no_sliding),
            };
            let g = client.create_group(&body).await?;
            format::render_group(&g, format);
        }
        GroupsCommand::Show { name } => {
            let g = client.get_group(&name).await?;
            format::render_group(&g, format);
        }
        GroupsCommand::Update(args) => {
            let body = PatchGroupBody {
                ttl_seconds: args.ttl_seconds,
                sliding_ttl: sliding_flag(args.sliding, args.no_sliding),
            };
            if body.ttl_seconds.is_none() && body.sliding_ttl.is_none() {
                return Err(ClientError::Validation(
                    "update requires at least one of --ttl-seconds, --sliding, --no-sliding".into(),
                ));
            }
            let g = client.patch_group(&args.name, &body).await?;
            format::render_group(&g, format);
        }
        GroupsCommand::Delete { name, force: _ } => {
            client.delete_group(&name).await?;
            if matches!(format, Format::Human) {
                println!("deleted {name}");
            }
        }
        GroupsCommand::Refresh { name } => {
            let g = client.refresh_group(&name).await?;
            format::render_group(&g, format);
        }
        GroupsCommand::State { name, clear } => {
            if !clear {
                return Err(ClientError::Validation(
                    "state listing isn't shipped yet — pass --clear to wipe state, or use \
                     `wm journal list` to see what handlers did"
                        .into(),
                ));
            }
            client.clear_group_state(&name).await?;
            if matches!(format, Format::Human) {
                println!("cleared state for {name}");
            }
        }
        GroupsCommand::Journal { name, clear } => {
            if !clear {
                return Err(ClientError::Validation(
                    "use `wm journal list <group>` to read; pass --clear to wipe".into(),
                ));
            }
            client.clear_group_journal(&name).await?;
            if matches!(format, Format::Human) {
                println!("cleared journal for {name}");
            }
        }
    }
    Ok(())
}

/// Resolve the boolean from `--sliding` / `--no-sliding` flags.
/// Returns `None` when neither is set (the API takes that as
/// "leave it alone" on patch / "use the default" on create).
fn sliding_flag(sliding: bool, no_sliding: bool) -> Option<bool> {
    match (sliding, no_sliding) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    }
}

// -- Routes ------------------------------------------------------------------

async fn handle_routes(
    client: &Client,
    cmd: RoutesCommand,
    format: Format,
) -> Result<(), ClientError> {
    match cmd {
        RoutesCommand::List => {
            let list = client.list_routes().await?;
            format::render_route_list(&list, format);
        }
        RoutesCommand::Add(args) => {
            let body = build_add_route_body(args)?;
            let r = client.create_route(&body).await?;
            format::render_route(&r, format);
        }
        RoutesCommand::Show { slug } => {
            let r = client.get_route(&slug).await?;
            format::render_route(&r, format);
        }
        RoutesCommand::Delete { slug, force: _ } => {
            client.delete_route(&slug).await?;
            if matches!(format, Format::Human) {
                println!("deleted {slug}");
            }
        }
    }
    Ok(())
}

fn build_add_route_body(args: AddRouteArgs) -> Result<CreateRouteBody, ClientError> {
    let methods: Vec<String> = args
        .method
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if methods.is_empty() {
        return Err(ClientError::Validation(
            "--method must be one or more comma-separated HTTP methods".into(),
        ));
    }
    match (args.source_file.as_deref(), args.wasm_file.as_deref()) {
        (Some(source_path), None) => {
            let source = read_to_string(source_path)?;
            Ok(CreateRouteBody {
                group: args.group,
                methods,
                path: args.path,
                language: args.language,
                bindings_version: None,
                compiled_wasm: None,
                source: Some(source),
            })
        }
        (None, Some(wasm_path)) => {
            let bytes = read_bytes(wasm_path)?;
            Ok(CreateRouteBody {
                group: args.group,
                methods,
                path: args.path,
                language: "wasm".into(),
                bindings_version: Some(args.bindings_version),
                compiled_wasm: Some(B64.encode(bytes)),
                source: None,
            })
        }
        (Some(_), Some(_)) => Err(ClientError::Validation(
            "pass exactly one of --source-file or --wasm-file".into(),
        )),
        (None, None) => Err(ClientError::Validation(
            "either --source-file or --wasm-file is required".into(),
        )),
    }
}

fn read_to_string(path: &str) -> Result<String, ClientError> {
    std::fs::read_to_string(Path::new(path))
        .map_err(|e| ClientError::Validation(format!("read {path}: {e}")))
}

fn read_bytes(path: &str) -> Result<Vec<u8>, ClientError> {
    std::fs::read(Path::new(path)).map_err(|e| ClientError::Validation(format!("read {path}: {e}")))
}

// -- Journal -----------------------------------------------------------------

async fn handle_journal(
    client: &Client,
    cmd: JournalCommand,
    format: Format,
) -> Result<(), ClientError> {
    match cmd {
        JournalCommand::List {
            group,
            before,
            limit,
        } => {
            let list = client.list_journal(&group, before, limit).await?;
            format::render_journal_list(&list, format);
        }
        JournalCommand::Show { slug } => {
            let (group, number) = parse_journal_slug(&slug)?;
            let entry = client.get_journal_entry(group, number).await?;
            format::render_journal_entry(&entry, format);
        }
    }
    Ok(())
}

/// Accept `{group}/journal/{n}` (canonical) or `{group}/{n}` (shorter).
fn parse_journal_slug(slug: &str) -> Result<(&str, u32), ClientError> {
    if let Some((group, rest)) = slug.split_once("/journal/") {
        let n = rest.parse::<u32>().map_err(|e| {
            ClientError::Validation(format!("journal slug must be {{group}}/journal/{{n}}: {e}"))
        })?;
        return Ok((group, n));
    }
    let (group, n) = slug.split_once('/').ok_or_else(|| {
        ClientError::Validation(format!(
            "journal slug must be 'group/journal/N' or 'group/N', got {slug:?}"
        ))
    })?;
    let n = n.parse::<u32>().map_err(|e| {
        ClientError::Validation(format!("journal slug 'group/N': N must be u32 ({e})"))
    })?;
    Ok((group, n))
}

// -- Tokens ------------------------------------------------------------------

async fn handle_tokens(
    client: &Client,
    cmd: TokensCommand,
    format: Format,
) -> Result<(), ClientError> {
    match cmd {
        TokensCommand::List => {
            let list = client.list_tokens().await?;
            format::render_token_list(&list, format);
        }
        TokensCommand::Create(CreateTokenArgs { name, ttl_seconds }) => {
            let body = CreateTokenBody { name, ttl_seconds };
            let resp = client.create_token(&body).await?;
            format::render_created_token(&resp.token, &resp.record, format);
        }
        TokensCommand::Revoke { name, force: _ } => {
            client.delete_token(&name).await?;
            if matches!(format, Format::Human) {
                println!("revoked {name}");
            }
        }
    }
    Ok(())
}

// -- Match probe -------------------------------------------------------------

async fn handle_match(
    client: &Client,
    method: &str,
    path: &str,
    format: Format,
) -> Result<(), ClientError> {
    let resp = client.match_route(method, path).await?;
    format::render_match(&resp, format);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_journal_slug_canonical() {
        assert_eq!(parse_journal_slug("g/journal/7").unwrap(), ("g", 7));
    }

    #[test]
    fn parse_journal_slug_short() {
        assert_eq!(parse_journal_slug("g/7").unwrap(), ("g", 7));
    }

    #[test]
    fn parse_journal_slug_rejects_garbage() {
        assert!(parse_journal_slug("nope").is_err());
        assert!(parse_journal_slug("g/journal/x").is_err());
    }

    #[test]
    fn sliding_flag_resolution() {
        assert_eq!(sliding_flag(false, false), None);
        assert_eq!(sliding_flag(true, false), Some(true));
        assert_eq!(sliding_flag(false, true), Some(false));
        // --sliding takes precedence if both are somehow set; clap
        // already enforces conflicts_with so this branch is defensive.
        assert_eq!(sliding_flag(true, true), Some(true));
    }
}
