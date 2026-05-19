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
    Client, ClientError, CreateGroupBody, CreateRouteBody, CreateTokenBody, CreateUserBody,
    DryRunBody, ListGroupsParams, ListJournalParams, ListRoutesParams, ListUnmatchedParams,
    PatchGroupBody, PatchRouteBody, PatchUserBody,
};

use crate::cli::{
    AddRouteArgs, Command, CreateGroupArgs, CreateTokenArgs, CreateUserArgs, ExportGroupArgs,
    GroupsCommand, GroupsListArgs, JournalCommand, JournalListArgs, RoutesCommand, RoutesListArgs,
    TestRouteArgs, TokensCommand, UnmatchedCommand, UnmatchedListArgs, UpdateRouteArgs,
    UpdateUserArgs, UsersCommand,
};
use crate::format::{self, Format};
use crate::spec::{self, LoadedSpec, SpecFormat};

/// Top-level dispatcher. Returns an `ExitCode` so the binary's `main`
/// can propagate. `Err(anyhow::Error)` is reserved for truly
/// unexpected client-side problems (failed to read a file, etc.) —
/// HTTP-level errors return `Ok(ExitCode)` after printing the
/// per-class message.
///
/// `host` and `token` are the values returned by `Config::resolve` —
/// they already have flag / env / profile / default layered into them.
pub async fn dispatch(
    host: String,
    token: Option<String>,
    json: bool,
    command: Command,
) -> anyhow::Result<ExitCode> {
    let format = Format::from_flag(json);

    // Three commands work without a token. Fast-path them so missing
    // token doesn't bother the user. (`wm match` does need a token —
    // it's a host-side probe, not a local computation.)
    // `wm completion` is purely local clap-derived and never touches
    // the host.
    let needs_token = !matches!(
        command,
        Command::Health | Command::Version | Command::Completion { .. }
    );
    if needs_token && token.is_none() {
        emit_error(
            format,
            "auth",
            "no token configured. Set WM_TOKEN, pass --token wmt_..., \
             or add `token = \"wmt_...\"` to the selected profile in \
             ~/.config/wiremirage/config.toml.",
        );
        return Ok(ExitCode::from(4));
    }

    let mut builder = Client::builder(&host);
    if let Some(token) = token.as_deref() {
        builder = builder.with_token(token);
    }
    let client = builder.build().context("build wm-core client")?;

    let result = match command {
        Command::Health => handle_health(&client, format).await,
        Command::Version => handle_version(&client, format).await,
        Command::Groups(cmd) => handle_groups(&client, cmd, format).await,
        Command::Routes(cmd) => handle_routes(&client, cmd, format).await,
        Command::Journal(cmd) => handle_journal(&client, cmd, format).await,
        Command::Unmatched(cmd) => handle_unmatched(&client, cmd, format).await,
        Command::Tokens(cmd) => handle_tokens(&client, cmd, format).await,
        Command::Users(cmd) => handle_users(&client, cmd, format).await,
        Command::Match { method, path } => handle_match(&client, &method, &path, format).await,
        Command::Completion { shell } => {
            handle_completion(shell);
            return Ok(ExitCode::from(0));
        }
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
        GroupsCommand::List(args) => {
            let params = groups_list_params(args);
            let list = client.list_groups_with(&params).await?;
            format::render_group_list(&list, format);
        }
        GroupsCommand::Create(args) => {
            handle_groups_create(client, args, format).await?;
        }
        GroupsCommand::Export(args) => {
            handle_groups_export(client, args).await?;
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

/// Top-level `wm groups create` dispatch. Branches on whether the
/// caller passed `--from-file` — without it, the existing
/// flag-driven flow runs; with it, the spec file is the source of
/// truth and ad-hoc flags like `--ttl-seconds` are ignored.
async fn handle_groups_create(
    client: &Client,
    args: CreateGroupArgs,
    format: Format,
) -> Result<(), ClientError> {
    match (args.from_file.as_deref(), args.name.as_deref()) {
        (Some(_), Some(_)) => Err(ClientError::Validation(
            "pass either <name> or --from-file FILE, not both".into(),
        )),
        (None, None) => Err(ClientError::Validation(
            "missing group name: pass <name> or use --from-file FILE".into(),
        )),
        (None, Some(name)) => {
            let body = CreateGroupBody {
                name: name.to_string(),
                ttl_seconds: args.ttl_seconds,
                sliding_ttl: sliding_flag(args.sliding, args.no_sliding),
            };
            let g = client.create_group(&body).await?;
            format::render_group(&g, format);
            Ok(())
        }
        (Some(from_file), None) => {
            let loaded = load_spec_from_arg(from_file, args.format.into())
                .map_err(|e| ClientError::Validation(format!("{e:#}")))?;
            create_group_from_spec(client, &loaded, format).await
        }
    }
}

/// Read the spec file from disk, or from stdin if path is "-".
fn load_spec_from_arg(path: &str, stdin_format: SpecFormat) -> anyhow::Result<LoadedSpec> {
    if path == "-" {
        use std::io::Read;
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        spec::load_spec_from_str(&text, stdin_format, None)
    } else {
        spec::load_spec_from_path(std::path::Path::new(path))
    }
}

/// Create a group + all its routes atomically from the spec. On any
/// failure mid-way through, the partially-created group is deleted
/// so the user is never left with a half-applied spec.
async fn create_group_from_spec(
    client: &Client,
    loaded: &LoadedSpec,
    format: Format,
) -> Result<(), ClientError> {
    let group_name = loaded.spec.name.clone();
    let create_body = CreateGroupBody {
        name: group_name.clone(),
        ttl_seconds: loaded.ttl_seconds,
        sliding_ttl: loaded.spec.sliding,
    };
    let group = client.create_group(&create_body).await?;

    for (idx, route) in loaded.normalized_routes.iter().enumerate() {
        let route_body = CreateRouteBody {
            group: Some(group_name.clone()),
            methods: route.methods.clone(),
            path: route.path.clone(),
            language: route.language.clone(),
            bindings_version: None,
            compiled_wasm: None,
            source: Some(route.source.clone()),
        };
        if let Err(e) = client.create_route(&route_body).await {
            // Best-effort rollback: delete the group we just made so
            // the user can re-run after fixing the spec. If the
            // rollback itself fails (network blip), surface both
            // errors so it's obvious.
            let rollback = client.delete_group(&group_name).await;
            let primary = format!(
                "route #{idx} ({path}) failed to create: {e}",
                path = route.path
            );
            let msg = match rollback {
                Ok(()) => format!(
                    "{primary}\n         group {group_name:?} was rolled back; re-run after fixing the spec"
                ),
                Err(rerr) => format!(
                    "{primary}\n         additionally, rollback failed: {rerr}; \
                     the group may exist with partial routes, delete it manually"
                ),
            };
            return Err(ClientError::Validation(msg));
        }
    }

    format::render_group(&group, format);
    if matches!(format, Format::Human) {
        println!(
            "created group {group_name:?} with {n} route(s) from spec",
            n = loaded.normalized_routes.len()
        );
    }
    Ok(())
}

/// `wm groups export` — read the group + routes + per-route source,
/// assemble a spec, render in the requested format. Wasm-uploaded
/// routes can't be represented (no source to put in the spec) and
/// fail the export so the user isn't surprised by a silent drop.
async fn handle_groups_export(client: &Client, args: ExportGroupArgs) -> Result<(), ClientError> {
    let group = client.get_group(&args.name).await?;

    // Fetch every route in this group. Bypassing pagination by
    // setting a high limit is fine — groups don't carry thousands
    // of routes in practice.
    let params = ListRoutesParams {
        group: Some(group.name.clone()),
        limit: Some(1000),
        ..Default::default()
    };
    let routes = client.list_routes_with(&params).await?;

    let mut route_specs = Vec::with_capacity(routes.routes.len());
    for route in &routes.routes {
        let slug = format!("{}/{}", route.group.name, route.number);
        let source_resp = client.get_route_source(&slug).await?;
        let Some(source) = source_resp.source else {
            return Err(ClientError::Validation(format!(
                "route {slug} was uploaded as pre-compiled {lang}; spec format requires source. \
                 Either replace the route with a source-uploaded version, or skip exporting this group.",
                lang = source_resp.language
            )));
        };

        let (method, methods) = match route.methods.as_slice() {
            [only] => (Some(only.clone()), Vec::new()),
            _ => (None, route.methods.clone()),
        };
        route_specs.push(spec::RouteSpec {
            method,
            methods,
            path: route.path.clone(),
            language: Some(route.language.clone()),
            source: Some(source),
            source_file: None,
        });
    }

    // Render with the TTL formatted as plain seconds; round-tripping
    // a parsed "24h" back as "86400" is correct but loses the human
    // form. We accept that trade-off rather than try to guess the
    // user's intended unit.
    let group_spec = spec::GroupSpec {
        name: group.name.clone(),
        description: None,
        ttl: Some(group.ttl_seconds.to_string()),
        sliding: Some(group.sliding_ttl),
        routes: route_specs,
    };

    let format: SpecFormat = args.format.into();
    let rendered = spec::render(&group_spec, format)
        .map_err(|e| ClientError::Validation(format!("render spec: {e:#}")))?;

    match args.output.as_deref() {
        Some(path) => std::fs::write(path, &rendered)
            .map_err(|e| ClientError::Validation(format!("write spec to {path}: {e}")))?,
        None => {
            print!("{rendered}");
            if !rendered.ends_with('\n') {
                println!();
            }
        }
    }
    Ok(())
}

// -- Routes ------------------------------------------------------------------

async fn handle_routes(
    client: &Client,
    cmd: RoutesCommand,
    format: Format,
) -> Result<(), ClientError> {
    match cmd {
        RoutesCommand::List(args) => {
            let params = routes_list_params(args);
            let list = client.list_routes_with(&params).await?;
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
        RoutesCommand::Update(args) => {
            let body = build_update_route_body(args)?;
            let slug = body.0;
            let r = client.patch_route(&slug, &body.1).await?;
            format::render_route(&r, format);
        }
        RoutesCommand::Source { slug } => {
            let resp = client.get_route_source(&slug).await?;
            format::render_route_source(&resp, format);
        }
        RoutesCommand::State { slug, clear } => {
            if clear {
                client.clear_route_state(&slug).await?;
                if matches!(format, Format::Human) {
                    println!("cleared state for {slug}");
                }
            } else {
                let list = client.list_route_state(&slug).await?;
                format::render_route_state(&list, format);
            }
        }
        RoutesCommand::Test(args) => {
            let (slug, body) = build_test_route_body(args)?;
            // The default request path is the route's own path. Fetch
            // the route first so the agent can `wm routes test slug`
            // without typing the path; supply `--path` to override.
            let body = fill_default_test_path(client, &slug, body).await?;
            let result = client.dry_run_route(&slug, &body).await?;
            format::render_dry_run(&result, format);
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

/// Translate `UpdateRouteArgs` into `(slug, PatchRouteBody)`. Returns
/// a usage error if no mutable field was supplied or if the
/// `--source-file` / `--wasm-file` combo is invalid.
fn build_update_route_body(args: UpdateRouteArgs) -> Result<(String, PatchRouteBody), ClientError> {
    let methods: Option<Vec<String>> = match args.method {
        Some(m) => {
            let list: Vec<String> = m
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if list.is_empty() {
                return Err(ClientError::Validation(
                    "--method must be one or more comma-separated HTTP methods".into(),
                ));
            }
            Some(list)
        }
        None => None,
    };

    let (language, bindings_version, compiled_wasm, source) =
        match (args.source_file.as_deref(), args.wasm_file.as_deref()) {
            (Some(path), None) => {
                let src = read_to_string(path)?;
                (Some(args.language), None, None, Some(src))
            }
            (None, Some(path)) => {
                let bytes = read_bytes(path)?;
                (
                    Some("wasm".to_string()),
                    Some(args.bindings_version),
                    Some(B64.encode(bytes)),
                    None,
                )
            }
            (Some(_), Some(_)) => {
                return Err(ClientError::Validation(
                    "pass at most one of --source-file or --wasm-file".into(),
                ));
            }
            (None, None) => (None, None, None, None),
        };

    if methods.is_none() && args.path.is_none() && compiled_wasm.is_none() && source.is_none() {
        return Err(ClientError::Validation(
            "wm routes update requires at least one of --method, --path, --source-file, --wasm-file"
                .into(),
        ));
    }

    Ok((
        args.slug,
        PatchRouteBody {
            methods,
            path: args.path,
            language,
            bindings_version,
            compiled_wasm,
            source,
        },
    ))
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

/// Translate `TestRouteArgs` into `(slug, DryRunBody)`. Headers and
/// path-params are parsed from their `KEY:VALUE` / `NAME=VALUE`
/// inline forms. Body accepts `@FILE` to load from disk.
fn build_test_route_body(args: TestRouteArgs) -> Result<(String, DryRunBody), ClientError> {
    let mut headers = Vec::with_capacity(args.headers.len());
    for raw in &args.headers {
        let (k, v) = raw.split_once(':').ok_or_else(|| {
            ClientError::Validation(format!("--header must be 'KEY: VALUE', got {raw:?}"))
        })?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let mut path_params = Vec::with_capacity(args.path_params.len());
    for raw in &args.path_params {
        let (k, v) = raw.split_once('=').ok_or_else(|| {
            ClientError::Validation(format!("--path-param must be 'NAME=VALUE', got {raw:?}"))
        })?;
        path_params.push((k.trim().to_string(), v.trim().to_string()));
    }
    let body = match args.body {
        Some(s) if s.starts_with('@') => read_bytes(&s[1..])?,
        Some(s) => s.into_bytes(),
        None => Vec::new(),
    };
    let kv_overrides = parse_override_pairs(&args.kv_overrides, "--kv")?;
    let gkv_overrides = parse_override_pairs(&args.gkv_overrides, "--gkv")?;
    Ok((
        args.slug,
        DryRunBody {
            method: args.method,
            path: args.path.unwrap_or_default(),
            headers,
            body,
            path_params: if path_params.is_empty() {
                None
            } else {
                Some(path_params)
            },
            query: Vec::new(),
            kv_overrides,
            gkv_overrides,
        },
    ))
}

/// Parse `KEY=VALUE` pairs from a CLI flag (e.g. `--kv counter=4`).
/// Trims whitespace around both key and value; rejects empty keys and
/// pairs without `=`.
fn parse_override_pairs(
    raw: &[String],
    flag_name: &str,
) -> Result<std::collections::HashMap<String, Vec<u8>>, ClientError> {
    let mut out = std::collections::HashMap::with_capacity(raw.len());
    for entry in raw {
        let (k, v) = entry.split_once('=').ok_or_else(|| {
            ClientError::Validation(format!("{flag_name} must be 'KEY=VALUE', got {entry:?}"))
        })?;
        let key = k.trim();
        if key.is_empty() {
            return Err(ClientError::Validation(format!(
                "{flag_name} has empty key: {entry:?}"
            )));
        }
        out.insert(key.to_string(), v.as_bytes().to_vec());
    }
    Ok(out)
}

/// Default the dry-run request path to the route's own path. The
/// agent typically wants "run this route's handler against itself";
/// supplying `--path` overrides this. One extra GET is a fine price
/// for an obvious default.
async fn fill_default_test_path(
    client: &Client,
    slug: &str,
    mut body: DryRunBody,
) -> Result<DryRunBody, ClientError> {
    if body.path.is_empty() {
        let route = client.get_route(slug).await?;
        body.path = route.path;
    }
    Ok(body)
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
        JournalCommand::List(args) => {
            let group = args.group.clone();
            let params = journal_list_params(args);
            let list = client.list_journal_with(&group, &params).await?;
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

fn groups_list_params(args: GroupsListArgs) -> ListGroupsParams {
    ListGroupsParams {
        owner_id: args.owner_id,
        name_prefix: args.name_prefix,
        q: args.q,
        since: args.since,
        until: args.until,
        implicit: args.implicit,
        sort: args.sort,
        dir: args.dir,
        offset: args.offset,
        limit: args.limit,
    }
}

fn routes_list_params(args: RoutesListArgs) -> ListRoutesParams {
    ListRoutesParams {
        group: args.group,
        owner_id: args.owner_id,
        method: args.method,
        path_pattern: args.path_pattern,
        since: args.since,
        until: args.until,
        q: args.q,
        sort: args.sort,
        dir: args.dir,
        offset: args.offset,
        limit: args.limit,
    }
}

fn journal_list_params(args: JournalListArgs) -> ListJournalParams {
    ListJournalParams {
        before: args.before,
        limit: args.limit,
        route: args.route,
        method: args.method,
        path_pattern: args.path_pattern,
        status: args.status,
        since: args.since,
        until: args.until,
    }
}

fn unmatched_list_params(args: UnmatchedListArgs) -> ListUnmatchedParams {
    ListUnmatchedParams {
        before: args.before,
        limit: args.limit,
        method: args.method,
        path_pattern: args.path_pattern,
        since: args.since,
        until: args.until,
    }
}

// -- Unmatched ----------------------------------------------------------------

async fn handle_unmatched(
    client: &Client,
    cmd: UnmatchedCommand,
    format: Format,
) -> Result<(), ClientError> {
    match cmd {
        UnmatchedCommand::List(args) => {
            let params = unmatched_list_params(args);
            let list = client.list_unmatched(&params).await?;
            format::render_unmatched_list(&list, format);
        }
        UnmatchedCommand::Show { number } => {
            let entry = client.get_unmatched_entry(number).await?;
            format::render_unmatched_entry(&entry, format);
        }
    }
    Ok(())
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

// -- Users -------------------------------------------------------------------

async fn handle_users(
    client: &Client,
    cmd: UsersCommand,
    format: Format,
) -> Result<(), ClientError> {
    match cmd {
        UsersCommand::List => {
            let list = client.list_users().await?;
            format::render_user_list(&list, format);
        }
        UsersCommand::Show { name } => {
            let user = client.get_user(&name).await?;
            format::render_user(&user, format);
        }
        UsersCommand::Me => {
            let user = client.get_me().await?;
            format::render_user(&user, format);
        }
        UsersCommand::Create(CreateUserArgs { name, admin }) => {
            let user = client
                .create_user(&CreateUserBody {
                    name,
                    is_admin: admin,
                })
                .await?;
            format::render_user(&user, format);
        }
        UsersCommand::Update(UpdateUserArgs {
            name,
            admin,
            no_admin,
        }) => {
            let body = PatchUserBody {
                is_admin: admin_flag(admin, no_admin),
            };
            if body.is_admin.is_none() {
                emit_error(
                    format,
                    "validation_failed",
                    "wm users update requires --admin or --no-admin",
                );
                return Ok(());
            }
            let user = client.patch_user(&name, &body).await?;
            format::render_user(&user, format);
        }
        UsersCommand::Delete { name, force: _ } => {
            client.delete_user(&name).await?;
            if matches!(format, Format::Human) {
                println!("deleted user {name}");
            }
        }
    }
    Ok(())
}

/// Resolves --admin / --no-admin into the optional bool used by the
/// PATCH body. None means "no change".
fn admin_flag(admin: bool, no_admin: bool) -> Option<bool> {
    if admin {
        Some(true)
    } else if no_admin {
        Some(false)
    } else {
        None
    }
}

// -- Shell completion --------------------------------------------------------

fn handle_completion(shell: clap_complete::Shell) {
    use clap::CommandFactory;
    let mut cmd = crate::cli::Cli::command();
    let bin_name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, bin_name, &mut std::io::stdout());
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

    #[test]
    fn parse_override_pairs_happy_path() {
        let parsed = parse_override_pairs(&["counter=4".into(), "name=alice".into()], "--kv")
            .expect("parse");
        assert_eq!(parsed.get("counter").unwrap(), b"4");
        assert_eq!(parsed.get("name").unwrap(), b"alice");
    }

    #[test]
    fn parse_override_pairs_trims_key_whitespace() {
        let parsed = parse_override_pairs(&["  counter  =4".into()], "--kv").expect("parse");
        assert!(parsed.contains_key("counter"));
    }

    #[test]
    fn parse_override_pairs_rejects_missing_equals() {
        let err = parse_override_pairs(&["nope".into()], "--kv").unwrap_err();
        assert!(format!("{err}").contains("--kv must be 'KEY=VALUE'"));
    }

    #[test]
    fn parse_override_pairs_rejects_empty_key() {
        let err = parse_override_pairs(&["=value".into()], "--kv").unwrap_err();
        assert!(format!("{err}").contains("empty key"));
    }

    #[test]
    fn parse_override_pairs_allows_equals_in_value() {
        // The value side may carry equals (e.g. base64 padding).
        let parsed = parse_override_pairs(&["k=a=b=c".into()], "--kv").expect("parse");
        assert_eq!(parsed.get("k").unwrap(), b"a=b=c");
    }
}
