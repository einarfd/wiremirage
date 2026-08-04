//! Group spec format — the shared YAML/JSON document for importing and
//! exporting a group's routes. Defined once here so every surface (the `wm`
//! CLI's `--from-file` / `export`, the host's REST import/export endpoints,
//! the MCP `import_group` / `export_group` tools, and the web UI) agrees on
//! the shape.
//!
//! A route entry carries either inline `source` or (CLI-only) `source_file`.
//! [`normalize`] here is **filesystem-free**: it requires inline `source` and
//! treats a `source_file` as an error. The CLI resolves `source_file` →
//! inline `source` against the spec file's directory before normalizing; the
//! host surfaces never see a `source_file` (no filesystem to resolve against).
//!
//! Scope is routes-only. The state-carrying *bundle* (kv/gkv + knob manifest,
//! ADR-0031) is deferred and is not represented here.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecFormat {
    Yaml,
    Json,
}

impl SpecFormat {
    /// Pick a format from a file extension. `None` when the extension isn't
    /// one we recognise — callers fall back to a user-supplied `--format`.
    pub fn from_extension(path: &Path) -> Option<Self> {
        match path.extension().and_then(|s| s.to_str()) {
            Some("yaml") | Some("yml") => Some(Self::Yaml),
            Some("json") => Some(Self::Json),
            _ => None,
        }
    }
}

/// On-disk / on-the-wire group spec.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GroupSpec {
    /// Group name (DNS label). Required.
    pub name: String,
    /// Duration string (`"24h"`, `"30m"`, `"86400"`). Plain integers are
    /// seconds. See [`parse_duration`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sliding: Option<bool>,
    /// Whether handlers in this group may make outbound callbacks (ADR-0034).
    /// Omitted when off (the default); present + `true` to opt the group in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callout: Option<bool>,
    #[serde(default)]
    pub routes: Vec<RouteSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteSpec {
    /// Singular convenience: `method: POST`. Mutually exclusive with
    /// `methods`; `normalize` folds either into the plural form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Inline handler source. Mutually exclusive with `source_file`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Path to a file containing the handler source, resolved relative to
    /// the spec file's directory — a **CLI-only** convenience. The CLI
    /// resolves it to inline `source` before [`normalize`]; the host
    /// rejects it (no filesystem).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_file: Option<String>,
}

/// A spec with its routes validated + folded into canonical form, ready to
/// create at the REST API. `source` is the literal inline source string.
#[derive(Debug, Clone)]
pub struct NormalizedSpec {
    pub name: String,
    pub ttl_seconds: Option<u64>,
    pub sliding: Option<bool>,
    pub callout: Option<bool>,
    pub routes: Vec<NormalizedRoute>,
}

#[derive(Debug, Clone)]
pub struct NormalizedRoute {
    pub methods: Vec<String>,
    pub path: String,
    pub language: String,
    pub source: String,
}

/// Result of importing a spec: the created group + how many routes landed.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ImportSummary {
    pub group: String,
    pub routes_created: usize,
}

/// Parse a spec from a raw string. Does not validate beyond deserialization
/// (and `deny_unknown_fields`); call [`normalize`] for semantic checks.
pub fn parse_str(text: &str, format: SpecFormat) -> Result<GroupSpec> {
    match format {
        SpecFormat::Yaml => serde_norway::from_str(text).context("parse YAML spec"),
        SpecFormat::Json => serde_json::from_str(text).context("parse JSON spec"),
    }
}

/// Validate + fold a spec into canonical form. **Filesystem-free**: every
/// route must carry inline `source` (a `source_file` is an error — resolve it
/// before calling this).
pub fn normalize(spec: &GroupSpec) -> Result<NormalizedSpec> {
    if spec.name.trim().is_empty() {
        return Err(anyhow!("spec is missing required field `name`"));
    }
    let ttl_seconds = match spec.ttl.as_deref() {
        Some(s) => Some(parse_duration(s)?),
        None => None,
    };
    let routes = spec
        .routes
        .iter()
        .enumerate()
        .map(|(idx, r)| normalize_route(idx, r))
        .collect::<Result<Vec<_>>>()?;
    Ok(NormalizedSpec {
        name: spec.name.clone(),
        ttl_seconds,
        sliding: spec.sliding,
        callout: spec.callout,
        routes,
    })
}

fn normalize_route(idx: usize, r: &RouteSpec) -> Result<NormalizedRoute> {
    // Methods: accept singular `method` OR plural `methods`, not both.
    let methods = match (&r.method, r.methods.is_empty()) {
        (Some(_), false) => {
            return Err(anyhow!(
                "route #{idx} ({path}): set exactly one of `method` or `methods`",
                path = r.path
            ));
        }
        (Some(m), true) => vec![m.clone()],
        (None, false) => r.methods.clone(),
        (None, true) => {
            return Err(anyhow!(
                "route #{idx} ({path}): missing `method` (or `methods`)",
                path = r.path
            ));
        }
    };

    // Source: exactly one of `source` / `source_file`, and `source_file` must
    // already be resolved to inline `source` before reaching here.
    let source = match (&r.source, &r.source_file) {
        (Some(_), Some(_)) => {
            return Err(anyhow!(
                "route #{idx} ({path}): set exactly one of `source` or `source_file`",
                path = r.path
            ));
        }
        (Some(s), None) => s.clone(),
        (None, Some(_)) => {
            return Err(anyhow!(
                "route #{idx} ({path}): `source_file` must be resolved to inline `source` before import",
                path = r.path
            ));
        }
        (None, None) => {
            return Err(anyhow!(
                "route #{idx} ({path}): set one of `source` (inline) or `source_file`",
                path = r.path
            ));
        }
    };

    let language = r
        .language
        .clone()
        .unwrap_or_else(|| "typescript".to_string());
    if r.path.trim().is_empty() {
        return Err(anyhow!("route #{idx}: missing `path`"));
    }

    Ok(NormalizedRoute {
        methods,
        path: r.path.clone(),
        language,
        source,
    })
}

/// Render a spec back to its on-disk form. Round-trips with [`parse_str`].
pub fn render(spec: &GroupSpec, format: SpecFormat) -> Result<String> {
    match format {
        SpecFormat::Yaml => serde_norway::to_string(spec).context("render YAML spec"),
        SpecFormat::Json => serde_json::to_string_pretty(spec).context("render JSON spec"),
    }
}

/// Parse a human duration string. Accepted forms:
///   * `"24h"`, `"30m"`, `"15s"`, `"7d"` — one unit.
///   * `"1h30m"`, `"2d12h"` — multiple units concatenated.
///   * `"86400"` — bare integer, interpreted as seconds.
///
/// Returns seconds. Errors on empty / non-matching / unknown unit / overflow.
pub fn parse_duration(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        return Err(anyhow!("empty duration"));
    }
    // Bare-integer fast path: "86400" → 86400 seconds.
    if let Ok(n) = s.parse::<u64>() {
        return Ok(n);
    }
    let mut total: u64 = 0;
    let mut current = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            current.push(ch);
            continue;
        }
        if current.is_empty() {
            return Err(anyhow!(
                "duration {s:?}: unit {ch:?} without a preceding number"
            ));
        }
        let n: u64 = current
            .parse()
            .map_err(|_| anyhow!("duration {s:?}: number {current:?} is too large"))?;
        let mult: u64 = match ch {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86400,
            other => return Err(anyhow!("duration {s:?}: unknown unit {other:?}")),
        };
        let segment = n
            .checked_mul(mult)
            .ok_or_else(|| anyhow!("duration {s:?}: {n}{ch} overflows u64"))?;
        total = total
            .checked_add(segment)
            .ok_or_else(|| anyhow!("duration {s:?}: total overflows u64"))?;
        current.clear();
    }
    if !current.is_empty() {
        return Err(anyhow!(
            "duration {s:?}: trailing number {current:?} has no unit"
        ));
    }
    Ok(total)
}

/// Format whole seconds as the most compact exact duration string that
/// [`parse_duration`] accepts: `86400` → `"1d"`, `3600` → `"1h"`, `90` →
/// `"90s"`. Used by `export_group` so an exported TTL stays human/agent-
/// friendly and round-trips back through `parse_duration` on import.
pub fn format_duration(seconds: u64) -> String {
    const DAY: u64 = 86_400;
    const HOUR: u64 = 3_600;
    const MIN: u64 = 60;
    if seconds != 0 && seconds.is_multiple_of(DAY) {
        format!("{}d", seconds / DAY)
    } else if seconds != 0 && seconds.is_multiple_of(HOUR) {
        format!("{}h", seconds / HOUR)
    } else if seconds != 0 && seconds.is_multiple_of(MIN) {
        format!("{}m", seconds / MIN)
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(text: &str, format: SpecFormat) -> NormalizedSpec {
        normalize(&parse_str(text, format).unwrap()).unwrap()
    }

    #[test]
    fn parses_minimal_yaml_with_inline_source() {
        let yaml = r#"
            name: stripe-mock
            routes:
              - method: POST
                path: /v1/charges
                source: "export default async function handle() {}"
        "#;
        let n = norm(yaml, SpecFormat::Yaml);
        assert_eq!(n.name, "stripe-mock");
        assert_eq!(n.routes.len(), 1);
        assert_eq!(n.routes[0].methods, vec!["POST".to_string()]);
        assert_eq!(n.routes[0].path, "/v1/charges");
        assert_eq!(n.routes[0].language, "typescript");
        assert!(n.routes[0].source.contains("handle"));
    }

    #[test]
    fn parses_minimal_json_with_inline_source() {
        let json =
            r#"{ "name": "g", "routes": [ { "method": "POST", "path": "/x", "source": "x" } ] }"#;
        let n = norm(json, SpecFormat::Json);
        assert_eq!(n.routes[0].source, "x");
    }

    #[test]
    fn plural_methods_field_works() {
        let yaml = "name: g\nroutes:\n  - methods: [POST, PUT]\n    path: /x\n    source: h\n";
        let n = norm(yaml, SpecFormat::Yaml);
        assert_eq!(
            n.routes[0].methods,
            vec!["POST".to_string(), "PUT".to_string()]
        );
    }

    #[test]
    fn rejects_method_and_methods_together() {
        let yaml =
            "name: g\nroutes:\n  - method: POST\n    methods: [PUT]\n    path: /x\n    source: h\n";
        let err = normalize(&parse_str(yaml, SpecFormat::Yaml).unwrap()).unwrap_err();
        assert!(format!("{err:#}").contains("method"));
    }

    #[test]
    fn rejects_source_and_source_file_together() {
        let yaml = "name: g\nroutes:\n  - method: POST\n    path: /x\n    source: a\n    source_file: ./h.ts\n";
        let err = normalize(&parse_str(yaml, SpecFormat::Yaml).unwrap()).unwrap_err();
        assert!(format!("{err:#}").contains("source"));
    }

    #[test]
    fn rejects_source_file_without_resolution() {
        // Host-side normalize is filesystem-free: a source_file is an error.
        let yaml = "name: g\nroutes:\n  - method: POST\n    path: /x\n    source_file: ./h.ts\n";
        let err = normalize(&parse_str(yaml, SpecFormat::Yaml).unwrap()).unwrap_err();
        assert!(format!("{err:#}").contains("source_file"));
    }

    #[test]
    fn rejects_neither_source() {
        let yaml = "name: g\nroutes:\n  - method: POST\n    path: /x\n";
        let err = normalize(&parse_str(yaml, SpecFormat::Yaml).unwrap()).unwrap_err();
        assert!(format!("{err:#}").contains("source"));
    }

    #[test]
    fn ttl_field_populates_ttl_seconds() {
        let yaml = "name: g\nttl: 24h\nroutes:\n  - method: POST\n    path: /x\n    source: h\n";
        assert_eq!(norm(yaml, SpecFormat::Yaml).ttl_seconds, Some(86_400));
    }

    #[test]
    fn missing_name_errors() {
        // `name` has no serde default, so deserialize rejects it before
        // normalize even runs — either layer is fine, the spec is rejected.
        let err = parse_str("ttl: 1h\nroutes: []\n", SpecFormat::Yaml).unwrap_err();
        assert!(format!("{err:#}").contains("name"));
    }

    #[test]
    fn empty_name_errors_in_normalize() {
        let spec = parse_str("name: \"\"\nroutes: []\n", SpecFormat::Yaml).unwrap();
        assert!(format!("{:#}", normalize(&spec).unwrap_err()).contains("name"));
    }

    #[test]
    fn unknown_field_is_rejected() {
        let yaml = "name: g\nwirremirage: 1\nroutes: []\n";
        let err = parse_str(yaml, SpecFormat::Yaml).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("wirremirage") || msg.contains("unknown"));
    }

    #[test]
    fn yaml_round_trip_preserves_shape() {
        let yaml = "name: stripe-mock\nttl: 1h\nsliding: true\nroutes:\n  - method: POST\n    path: /v1/charges\n    source: inline\n";
        let spec = parse_str(yaml, SpecFormat::Yaml).unwrap();
        let rendered = render(&spec, SpecFormat::Yaml).unwrap();
        let again = parse_str(&rendered, SpecFormat::Yaml).unwrap();
        assert_eq!(again.name, "stripe-mock");
        assert_eq!(again.ttl.as_deref(), Some("1h"));
        assert_eq!(again.sliding, Some(true));
        let n = normalize(&again).unwrap();
        assert_eq!(n.routes.len(), 1);
        assert_eq!(n.routes[0].source, "inline");
    }

    #[test]
    fn callout_round_trips_and_defaults_off() {
        // Present + true survives parse → render → parse → normalize.
        let yaml = "name: g\ncallout: true\nroutes: []\n";
        let again = {
            let spec = parse_str(yaml, SpecFormat::Yaml).unwrap();
            let rendered = render(&spec, SpecFormat::Yaml).unwrap();
            parse_str(&rendered, SpecFormat::Yaml).unwrap()
        };
        assert_eq!(again.callout, Some(true));
        assert_eq!(normalize(&again).unwrap().callout, Some(true));

        // Absent → None → import treats it as off (the default).
        let bare = parse_str("name: g\nroutes: []\n", SpecFormat::Yaml).unwrap();
        assert_eq!(bare.callout, None);
        assert_eq!(normalize(&bare).unwrap().callout, None);
    }

    #[test]
    fn duration_parses_single_units() {
        assert_eq!(parse_duration("60").unwrap(), 60);
        assert_eq!(parse_duration("30s").unwrap(), 30);
        assert_eq!(parse_duration("5m").unwrap(), 300);
        assert_eq!(parse_duration("24h").unwrap(), 86_400);
        assert_eq!(parse_duration("7d").unwrap(), 604_800);
    }

    #[test]
    fn duration_parses_compound_units() {
        assert_eq!(parse_duration("1h30m").unwrap(), 5400);
        assert_eq!(parse_duration("2d12h").unwrap(), 216_000);
    }

    #[test]
    fn format_duration_picks_compact_unit_and_round_trips() {
        assert_eq!(super::format_duration(86_400), "1d");
        assert_eq!(super::format_duration(3_600), "1h");
        assert_eq!(super::format_duration(90), "90s");
        assert_eq!(super::format_duration(120), "2m");
        assert_eq!(super::format_duration(0), "0s");
        // Round-trips back through parse_duration (export → import).
        for secs in [1u64, 59, 60, 3_600, 5_400, 86_400, 172_800] {
            assert_eq!(parse_duration(&super::format_duration(secs)).unwrap(), secs);
        }
    }

    #[test]
    fn duration_rejects_garbage() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("h").is_err());
        assert!(parse_duration("30x").is_err());
        assert!(parse_duration("h30").is_err());
        assert!(parse_duration("1h30").is_err());
    }
}
