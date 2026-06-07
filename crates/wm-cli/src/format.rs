//! Human-readable output for list and show responses, plus the JSON
//! switch. List commands render as column-aligned text tables (no
//! external dep — the rendering is straightforward); show commands
//! render as labeled key-value blocks. JSON output uses
//! `serde_json::to_string_pretty` so it's diffable and `jq`-friendly.
//!
//! The agent skill teaches `--json` for scripting; this module is
//! the human side. Color is intentionally off in this slice — agents
//! mostly read uncolored output anyway.

use serde::Serialize;
use wm_core::{
    DryRunResult, GroupRecord, HealthResponse, JournalRecord, ListGroupsResponse,
    ListJournalResponse, ListRouteStateResponse, ListRoutesResponse, ListTokensResponse,
    ListUnmatchedResponse, ListUsersResponse, MatchResponse, NearMiss, NearMissReason, RouteRecord,
    RouteSourceResponse, TokenRecord, UnmatchedRecord, UserRecord,
};

/// Output mode requested via the global `--json` flag.
#[derive(Debug, Clone, Copy)]
pub enum Format {
    Human,
    Json,
}

impl Format {
    pub fn from_flag(json: bool) -> Self {
        if json { Self::Json } else { Self::Human }
    }
}

/// Print a value as JSON (pretty-printed). Used by the json arms of
/// each command.
pub fn print_json<T: Serialize>(value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("error: failed to serialize JSON: {e}"),
    }
}

// -- Health ------------------------------------------------------------------

pub fn render_health(h: &HealthResponse, format: Format) {
    match format {
        Format::Json => print_json(h),
        Format::Human => println!("status: {}\nversion: {}", h.status, h.version),
    }
}

// -- Groups ------------------------------------------------------------------

pub fn render_group(g: &GroupRecord, format: Format) {
    match format {
        Format::Json => print_json(g),
        Format::Human => {
            println!("name:        {}", g.name);
            println!("id:          {}", g.id);
            println!("owner:       {}", g.owner_id);
            println!("ttl_seconds: {}", g.ttl_seconds);
            println!("sliding:     {}", g.sliding_ttl);
            println!("implicit:    {}", g.implicit);
            println!("created_at:  {}", g.created_at);
        }
    }
}

pub fn render_group_list(list: &ListGroupsResponse, format: Format) {
    match format {
        Format::Json => print_json(list),
        Format::Human => {
            if list.groups.is_empty() {
                println!("(no groups)");
                print_pagination_footer(list.groups.len(), list.total, list.next_offset);
                return;
            }
            let rows: Vec<[String; 5]> = list
                .groups
                .iter()
                .map(|g| {
                    [
                        g.name.clone(),
                        g.ttl_seconds.to_string(),
                        if g.implicit {
                            "yes".into()
                        } else {
                            "no".into()
                        },
                        g.last_activity_at.clone().unwrap_or_else(|| "-".into()),
                        g.created_at.clone(),
                    ]
                })
                .collect();
            print_table(
                &["NAME", "TTL_S", "IMPLICIT", "LAST_ACTIVITY", "CREATED_AT"],
                &rows,
            );
            print_pagination_footer(list.groups.len(), list.total, list.next_offset);
        }
    }
}

// -- Routes ------------------------------------------------------------------

pub fn render_route(r: &RouteRecord, format: Format) {
    match format {
        Format::Json => print_json(r),
        Format::Human => {
            println!("slug:             {}/{}", r.group.name, r.number);
            println!("id:               {}", r.id);
            println!("group:            {} ({})", r.group.name, r.group.id);
            println!("methods:          {}", r.methods.join(", "));
            println!("path:             {}", r.path);
            if let Some(url) = &r.url {
                println!("url:              {url}");
            }
            println!("language:         {}", r.language);
            println!("bindings_version: {}", r.bindings_version);
            println!("owner:            {}", r.owner_id);
            println!("created_at:       {}", r.created_at);
        }
    }
}

pub fn render_route_list(list: &ListRoutesResponse, format: Format) {
    match format {
        Format::Json => print_json(list),
        Format::Human => {
            if list.routes.is_empty() {
                println!("(no routes)");
                print_pagination_footer(list.routes.len(), list.total, list.next_offset);
                return;
            }
            let rows: Vec<[String; 6]> = list
                .routes
                .iter()
                .map(|r| {
                    [
                        format!("{}/{}", r.group.name, r.number),
                        r.methods.join(","),
                        r.path.clone(),
                        r.language.clone(),
                        r.hits_total.to_string(),
                        r.last_hit_at.clone().unwrap_or_else(|| "-".into()),
                    ]
                })
                .collect();
            print_table(
                &["SLUG", "METHODS", "PATH", "LANG", "HITS", "LAST_HIT"],
                &rows,
            );
            print_pagination_footer(list.routes.len(), list.total, list.next_offset);
        }
    }
}

// -- Journal -----------------------------------------------------------------

pub fn render_journal_entry(j: &JournalRecord, format: Format) {
    match format {
        Format::Json => print_json(j),
        Format::Human => {
            println!("slug:        {}/journal/{}", j.group_name, j.number);
            println!("trace_id:    {}", j.trace_id.as_deref().unwrap_or("-"));
            println!(
                "request:     {} {} ({} bytes{})",
                j.request.method,
                j.request.path,
                j.request.original_body_size,
                if j.request.body_truncated {
                    ", truncated"
                } else {
                    ""
                }
            );
            println!(
                "response:    {} ({} bytes{})",
                j.response.status,
                j.response.original_body_size,
                if j.response.body_truncated {
                    ", truncated"
                } else {
                    ""
                }
            );
            println!("matched:     {}", j.matched_pattern);
            println!("duration_ms: {}", j.duration_ms);
            if let Some(err) = &j.error {
                println!("error:       {err}");
            }
            if !j.handler_logs.is_empty() {
                println!("handler logs:");
                for entry in &j.handler_logs {
                    println!("  [{}] {}: {}", entry.timestamp, entry.level, entry.message);
                }
            }
        }
    }
}

pub fn render_journal_list(list: &ListJournalResponse, format: Format) {
    match format {
        Format::Json => print_json(list),
        Format::Human => {
            if list.entries.is_empty() {
                println!("(no journal entries)");
                return;
            }
            let rows: Vec<[String; 5]> = list
                .entries
                .iter()
                .map(|e| {
                    [
                        e.number.to_string(),
                        e.request.method.clone(),
                        e.request.path.clone(),
                        e.response.status.to_string(),
                        format!("{} ms", e.duration_ms),
                    ]
                })
                .collect();
            print_table(&["NUMBER", "METHOD", "PATH", "STATUS", "DURATION"], &rows);
            if let Some(b) = list.next_before {
                println!("\n(next page: --before={b})");
            }
        }
    }
}

// -- Unmatched ---------------------------------------------------------------

pub fn render_unmatched_list(list: &ListUnmatchedResponse, format: Format) {
    match format {
        Format::Json => print_json(list),
        Format::Human => {
            if list.entries.is_empty() {
                println!("(no unmatched entries)");
                return;
            }
            let rows: Vec<[String; 5]> = list
                .entries
                .iter()
                .map(|e| {
                    [
                        e.number.to_string(),
                        e.group_name.clone(),
                        e.request.method.clone(),
                        e.request.path.clone(),
                        e.created_at.to_rfc3339(),
                    ]
                })
                .collect();
            print_table(&["NUMBER", "GROUP", "METHOD", "PATH", "WHEN"], &rows);
            if let Some(b) = list.next_before {
                println!("\n(next page: --before={b})");
            }
        }
    }
}

pub fn render_unmatched_entry(u: &UnmatchedRecord, format: Format) {
    match format {
        Format::Json => print_json(u),
        Format::Human => {
            println!("number:     {}", u.number);
            println!("group:      {}", u.group_name);
            println!("trace_id:   {}", u.trace_id.as_deref().unwrap_or("-"));
            println!("when:       {}", u.created_at);
            println!(
                "request:    {} {} ({} bytes{})",
                u.request.method,
                u.request.path,
                u.request.original_body_size,
                if u.request.body_truncated {
                    ", truncated"
                } else {
                    ""
                }
            );
            if !u.near_misses.is_empty() {
                println!("near_misses:");
                for nm in &u.near_misses {
                    let reason = match &nm.reason {
                        wm_core::UnmatchedNearMissReason::MethodMismatch {
                            expected_methods,
                            ..
                        } => format!(
                            "method_mismatch (expected: {})",
                            expected_methods.join(", ")
                        ),
                        wm_core::UnmatchedNearMissReason::PrefixMatch { expected, got, .. } => {
                            format!("prefix_match (expected: {expected}, got: {got})")
                        }
                    };
                    println!(
                        "  {slug} {methods} {path} — {reason}",
                        slug = nm.route,
                        methods = nm.route_methods.join(","),
                        path = nm.route_path,
                    );
                }
            }
        }
    }
}

// -- Tokens ------------------------------------------------------------------

pub fn render_token_list(list: &ListTokensResponse, format: Format) {
    match format {
        Format::Json => print_json(list),
        Format::Human => {
            if list.tokens.is_empty() {
                println!("(no tokens)");
                return;
            }
            let rows: Vec<[String; 4]> = list
                .tokens
                .iter()
                .map(|t| {
                    [
                        t.name.clone(),
                        t.created_at.clone(),
                        t.expires_at.clone().unwrap_or_else(|| "-".into()),
                        t.last_used_at.clone().unwrap_or_else(|| "-".into()),
                    ]
                })
                .collect();
            print_table(&["NAME", "CREATED_AT", "EXPIRES_AT", "LAST_USED_AT"], &rows);
        }
    }
}

pub fn render_created_token(plaintext: &str, record: &TokenRecord, format: Format) {
    match format {
        Format::Json => {
            // Match the API shape so scripts can decode the plaintext
            // out of the same field.
            print_json(&serde_json::json!({
                "token": plaintext,
                "record": record,
            }));
        }
        Format::Human => {
            println!("Save this now; it won't be shown again.");
            println!();
            println!("token:      {plaintext}");
            println!("name:       {}", record.name);
            println!("created_at: {}", record.created_at);
            if let Some(exp) = &record.expires_at {
                println!("expires_at: {exp}");
            }
        }
    }
}

// -- Generic table helper ----------------------------------------------------

/// Render a fixed-column table. Trailing whitespace on the last column
/// is not stripped (jq-friendly to leave it consistent).
fn print_table<const N: usize>(headers: &[&str; N], rows: &[[String; N]]) {
    let mut widths = [0usize; N];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = h.len();
    }
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if cell.len() > widths[i] {
                widths[i] = cell.len();
            }
        }
    }
    print_row(headers, &widths);
    for row in rows {
        print_row(&row.iter().map(String::as_str).collect::<Vec<_>>(), &widths);
    }
}

/// Footer for offset-paginated list responses. Prints
/// `(showing K of N; --offset M for the next page)` when there are
/// further results, or `(showing K of N)` when this is the last page.
/// Skipped entirely when `total == 0` and nothing is shown.
fn print_pagination_footer(shown: usize, total: u64, next_offset: Option<u64>) {
    if total == 0 {
        return;
    }
    match next_offset {
        Some(off) => println!("\n(showing {shown} of {total}; --offset {off} for the next page)"),
        None => println!("\n(showing {shown} of {total})"),
    }
}

fn print_row<S: AsRef<str>>(cells: &[S], widths: &[usize]) {
    let mut line = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        let s = cell.as_ref();
        line.push_str(s);
        for _ in s.len()..widths[i] {
            line.push(' ');
        }
    }
    // Trim trailing whitespace on the rendered line (only the very
    // last column would have it).
    println!("{}", line.trim_end());
}

// -- Users -------------------------------------------------------------------

pub fn render_user(u: &UserRecord, format: Format) {
    match format {
        Format::Json => print_json(u),
        Format::Human => {
            println!("name:       {}", u.name);
            println!("id:         {}", u.id);
            println!("admin:      {}", if u.is_admin { "yes" } else { "no" });
            println!("created_at: {}", u.created_at);
        }
    }
}

pub fn render_user_list(list: &ListUsersResponse, format: Format) {
    match format {
        Format::Json => print_json(list),
        Format::Human => {
            if list.users.is_empty() {
                println!("(no users)");
                return;
            }
            let rows: Vec<[String; 3]> = list
                .users
                .iter()
                .map(|u| {
                    [
                        u.name.clone(),
                        if u.is_admin {
                            "yes".into()
                        } else {
                            "no".into()
                        },
                        u.created_at.clone(),
                    ]
                })
                .collect();
            print_table(&["NAME", "ADMIN", "CREATED_AT"], &rows);
        }
    }
}

// -- Match probe -------------------------------------------------------------

pub fn render_match(resp: &MatchResponse, format: Format) {
    match format {
        Format::Json => print_json(resp),
        Format::Human => match resp {
            MatchResponse::Hit(hit) => {
                let r = &hit.route;
                println!(
                    "matched {}/{} ({} {})",
                    r.group.name,
                    r.number,
                    r.methods.join(","),
                    r.path,
                );
                if !hit.path_params.is_empty() {
                    println!("path_params:");
                    for (k, v) in &hit.path_params {
                        println!("  {k} = {v}");
                    }
                }
            }
            MatchResponse::Miss(miss) => {
                if miss.near_misses.is_empty() {
                    println!("no match, and no near-misses found");
                    return;
                }
                println!("no match. near-misses:");
                for nm in &miss.near_misses {
                    println!("  - {} ({})", nm.route, nm.route_path);
                    print_near_miss_reason(nm);
                }
            }
        },
    }
}

fn print_near_miss_reason(nm: &NearMiss) {
    match nm.reason {
        NearMissReason::MethodMismatch => {
            let expected = nm
                .details
                .get("expected_methods")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let got = nm
                .details
                .get("got")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            println!("    reason: method_mismatch (expected: [{expected}], got: {got})");
        }
        NearMissReason::PrefixMatch => {
            let idx = nm
                .details
                .get("segment_index")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let expected = nm
                .details
                .get("expected")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let got = nm
                .details
                .get("got")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            println!(
                "    reason: prefix_match (segment {idx}: expected {expected:?}, got {got:?})"
            );
        }
    }
}

// -- Route state -------------------------------------------------------------

pub fn render_route_state(list: &ListRouteStateResponse, format: Format) {
    match format {
        Format::Json => print_json(list),
        Format::Human => {
            if list.entries.is_empty() {
                println!("(no state)");
                return;
            }
            let rows: Vec<[String; 3]> = list
                .entries
                .iter()
                .map(|e| {
                    let detail = match e.kind.as_str() {
                        "bytes" => match &e.value {
                            Some(v) => match std::str::from_utf8(v) {
                                Ok(s) => s.to_string(),
                                Err(_) => format!("<{} bytes>", v.len()),
                            },
                            None => "<empty>".into(),
                        },
                        "list" | "hash" | "set" => {
                            format!("len={}", e.length.unwrap_or(0))
                        }
                        _ => "(unknown type)".into(),
                    };
                    [e.key.clone(), e.kind.clone(), detail]
                })
                .collect();
            print_table(&["KEY", "KIND", "VALUE"], &rows);
        }
    }
}

// -- Source -------------------------------------------------------------------

pub fn render_route_source(resp: &RouteSourceResponse, format: Format) {
    match format {
        Format::Json => print_json(resp),
        Format::Human => match &resp.source {
            Some(src) => print!("{src}"),
            None => println!(
                "(no source stored — route was uploaded as pre-compiled `{}`)",
                resp.language
            ),
        },
    }
}

// -- Dry-run -----------------------------------------------------------------

pub fn render_dry_run(r: &DryRunResult, format: Format) {
    match format {
        Format::Json => print_json(r),
        Format::Human => {
            println!("status: {}", r.status);
            println!("duration_ms: {}", r.duration_ms);
            println!("snapshot_keys: {}", r.snapshot_keys);
            if let Some(err) = &r.error {
                println!("error: {err}");
            }
            if !r.headers.is_empty() {
                println!("headers:");
                for (k, v) in &r.headers {
                    println!("  {k}: {v}");
                }
            }
            if !r.body.is_empty() {
                match std::str::from_utf8(&r.body) {
                    Ok(s) => println!("body:\n{s}"),
                    Err(_) => println!("body: <{} non-utf8 bytes>", r.body.len()),
                }
            }
            if !r.handler_logs.is_empty() {
                println!("logs:");
                for log in &r.handler_logs {
                    println!("  [{}] {}", log.level, log.message);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // Output rendering is exercised end-to-end by the tier-3 binary
    // tests; the helpers here are pure formatting and a manual code
    // review keeps them honest. No unit tests for now.
}
