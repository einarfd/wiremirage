//! Group spec files — the YAML / JSON document `wm groups create
//! --from-file` reads and `wm groups export` writes.
//!
//! Format per [[cli-design.md]]'s "Group spec files" section. The
//! same shape round-trips through import and export so CI workflows
//! can keep `mocks/foo.yaml` in version control and re-apply it on
//! every test run.
//!
//! A route entry has either `source_file` (path resolved relative to
//! the spec file) or `source` (inline string). Exactly one — both is
//! an error, neither is an error.
//!
//! On import, paths in `source_file` are resolved relative to the
//! spec file's directory. Reading from stdin (`--from-file -`)
//! forbids `source_file` since there is no directory to be relative
//! to.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecFormat {
    Yaml,
    Json,
}

impl SpecFormat {
    /// Pick a format from a file extension. Returns `None` when the
    /// extension isn't one we recognise — callers either error out or
    /// fall back to the user-supplied `--format` flag.
    pub fn from_extension(path: &Path) -> Option<Self> {
        match path.extension().and_then(|s| s.to_str()) {
            Some("yaml") | Some("yml") => Some(Self::Yaml),
            Some("json") => Some(Self::Json),
            _ => None,
        }
    }
}

/// On-disk shape. Mirrors the example in `cli-design.md`. After
/// `normalize_route` runs, every `RouteSpec.methods` is non-empty and
/// `method` is `None` — callers downstream see one canonical form.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupSpec {
    pub name: String,
    /// Free-form description carried through round-trips. The host
    /// data model doesn't persist this today, so it survives the
    /// spec file but is lost the moment the group is created. We
    /// keep parsing it to avoid breaking authored specs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Duration string (e.g. `"24h"`, `"30m"`, `"86400"`). Plain
    /// integers are interpreted as seconds. See `parse_duration`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sliding: Option<bool>,
    #[serde(default)]
    pub routes: Vec<RouteSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteSpec {
    /// Singular convenience: `method: POST`. Mutually exclusive with
    /// `methods` on the wire; `normalize_route` folds either form
    /// into the plural `methods` list.
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
    /// Path to a file containing the handler source, resolved
    /// relative to the spec file's directory. Mutually exclusive
    /// with `source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_file: Option<String>,
}

/// A spec loaded from disk plus the (resolved, validated) routes that
/// are ready to ship at the REST API. The original `GroupSpec` is
/// kept for reference only — `normalized_routes` is what callers use.
#[derive(Debug)]
pub struct LoadedSpec {
    pub spec: GroupSpec,
    /// One entry per route in `spec.routes`, in the same order.
    /// Methods are non-empty; `source` is the literal source string
    /// (inline or read from `source_file`).
    pub normalized_routes: Vec<NormalizedRoute>,
    pub ttl_seconds: Option<u64>,
}

#[derive(Debug)]
pub struct NormalizedRoute {
    pub methods: Vec<String>,
    pub path: String,
    pub language: String,
    pub source: String,
}

/// Read and normalize a spec file. `--from-file -` is handled by
/// `load_spec_from_str` so callers can read stdin directly.
pub fn load_spec_from_path(path: &Path) -> Result<LoadedSpec> {
    let format = SpecFormat::from_extension(path).ok_or_else(|| {
        anyhow!(
            "spec file {} has no recognised extension; expected .yaml, .yml, or .json (or use `--from-file -` to read stdin with `--format`)",
            path.display()
        )
    })?;
    let text =
        fs::read_to_string(path).with_context(|| format!("read spec file {}", path.display()))?;
    let base_dir = path.parent().map(Path::to_path_buf);
    parse_and_normalize(&text, format, base_dir.as_deref())
}

/// Read and normalize a spec from a raw string (stdin path).
/// `base_dir = None` means `source_file` references are forbidden —
/// there's no directory to be relative to.
pub fn load_spec_from_str(
    text: &str,
    format: SpecFormat,
    base_dir: Option<&Path>,
) -> Result<LoadedSpec> {
    parse_and_normalize(text, format, base_dir)
}

fn parse_and_normalize(
    text: &str,
    format: SpecFormat,
    base_dir: Option<&Path>,
) -> Result<LoadedSpec> {
    let spec: GroupSpec = match format {
        SpecFormat::Yaml => serde_yml::from_str(text).context("parse YAML spec")?,
        SpecFormat::Json => serde_json::from_str(text).context("parse JSON spec")?,
    };
    if spec.name.trim().is_empty() {
        return Err(anyhow!("spec is missing required field `name`"));
    }
    let ttl_seconds = match spec.ttl.as_deref() {
        Some(s) => Some(parse_duration(s)?),
        None => None,
    };
    let normalized_routes = spec
        .routes
        .iter()
        .enumerate()
        .map(|(idx, r)| normalize_route(idx, r, base_dir))
        .collect::<Result<Vec<_>>>()?;
    Ok(LoadedSpec {
        spec,
        normalized_routes,
        ttl_seconds,
    })
}

fn normalize_route(idx: usize, r: &RouteSpec, base_dir: Option<&Path>) -> Result<NormalizedRoute> {
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

    // Source: exactly one of `source` / `source_file`.
    let source = match (&r.source, &r.source_file) {
        (Some(_), Some(_)) => {
            return Err(anyhow!(
                "route #{idx} ({path}): set exactly one of `source` or `source_file`",
                path = r.path
            ));
        }
        (Some(s), None) => s.clone(),
        (None, Some(rel)) => {
            let Some(base) = base_dir else {
                return Err(anyhow!(
                    "route #{idx} ({path}): `source_file` is forbidden when the spec is read from stdin; use inline `source` instead",
                    path = r.path
                ));
            };
            let resolved: PathBuf = base.join(rel);
            fs::read_to_string(&resolved).with_context(|| {
                format!(
                    "route #{idx} ({path}): read source_file {}",
                    resolved.display(),
                    path = r.path
                )
            })?
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

/// Parse a human duration string. Accepted forms:
///   * `"24h"`, `"30m"`, `"15s"`, `"7d"` — one unit.
///   * `"1h30m"`, `"2d12h"` — multiple units concatenated.
///   * `"86400"` — bare integer, interpreted as seconds.
///
/// Returns the duration in seconds. Errors on empty / non-matching /
/// unknown unit / numeric overflow.
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

/// Render a spec back to its on-disk form. Inverse of `parse_and_normalize`
/// from the perspective of round-trip: a spec exported this way and re-
/// imported produces equivalent `LoadedSpec` content.
pub fn render(spec: &GroupSpec, format: SpecFormat) -> Result<String> {
    match format {
        SpecFormat::Yaml => serde_yml::to_string(spec).context("render YAML spec"),
        SpecFormat::Json => serde_json::to_string_pretty(spec).context("render JSON spec"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_yaml_with_inline_source() {
        let yaml = r#"
            name: stripe-mock
            routes:
              - method: POST
                path: /v1/charges
                source: |
                  export default async function handle() {}
        "#;
        let loaded = load_spec_from_str(yaml, SpecFormat::Yaml, None).unwrap();
        assert_eq!(loaded.spec.name, "stripe-mock");
        assert_eq!(loaded.normalized_routes.len(), 1);
        let r = &loaded.normalized_routes[0];
        assert_eq!(r.methods, vec!["POST".to_string()]);
        assert_eq!(r.path, "/v1/charges");
        assert_eq!(r.language, "typescript");
        assert!(r.source.contains("handle"));
    }

    #[test]
    fn parses_minimal_json_with_inline_source() {
        let json = r#"
            {
              "name": "stripe-mock",
              "routes": [
                { "method": "POST", "path": "/v1/charges", "source": "x" }
              ]
            }
        "#;
        let loaded = load_spec_from_str(json, SpecFormat::Json, None).unwrap();
        assert_eq!(loaded.spec.name, "stripe-mock");
        assert_eq!(loaded.normalized_routes[0].source, "x");
    }

    #[test]
    fn plural_methods_field_works() {
        let yaml = r#"
            name: g
            routes:
              - methods: [POST, PUT]
                path: /x
                source: handler
        "#;
        let loaded = load_spec_from_str(yaml, SpecFormat::Yaml, None).unwrap();
        assert_eq!(
            loaded.normalized_routes[0].methods,
            vec!["POST".to_string(), "PUT".to_string()]
        );
    }

    #[test]
    fn rejects_method_and_methods_together() {
        let yaml = r#"
            name: g
            routes:
              - method: POST
                methods: [PUT]
                path: /x
                source: handler
        "#;
        let err = load_spec_from_str(yaml, SpecFormat::Yaml, None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("method"), "names the conflict: {msg}");
    }

    #[test]
    fn rejects_source_and_source_file_together() {
        let yaml = r#"
            name: g
            routes:
              - method: POST
                path: /x
                source: inline
                source_file: ./handler.ts
        "#;
        let err = load_spec_from_str(yaml, SpecFormat::Yaml, None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("source"), "names the conflict: {msg}");
    }

    #[test]
    fn rejects_neither_source_nor_source_file() {
        let yaml = r#"
            name: g
            routes:
              - method: POST
                path: /x
        "#;
        let err = load_spec_from_str(yaml, SpecFormat::Yaml, None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("source"), "names the missing field: {msg}");
    }

    #[test]
    fn stdin_input_forbids_source_file() {
        let yaml = r#"
            name: g
            routes:
              - method: POST
                path: /x
                source_file: ./handler.ts
        "#;
        let err = load_spec_from_str(yaml, SpecFormat::Yaml, None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("stdin"), "explains the restriction: {msg}");
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
    fn duration_rejects_garbage() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("h").is_err());
        assert!(parse_duration("30x").is_err());
        assert!(parse_duration("30").map(|n| n == 30).unwrap_or(false));
        assert!(parse_duration("h30").is_err());
        assert!(parse_duration("1h30").is_err()); // trailing digits, no unit
    }

    #[test]
    fn ttl_field_populates_ttl_seconds() {
        let yaml = r#"
            name: g
            ttl: 24h
            routes:
              - method: POST
                path: /x
                source: handler
        "#;
        let loaded = load_spec_from_str(yaml, SpecFormat::Yaml, None).unwrap();
        assert_eq!(loaded.ttl_seconds, Some(86_400));
    }

    #[test]
    fn yaml_round_trip_preserves_shape() {
        let yaml = r#"
            name: stripe-mock
            description: Stripe mock
            ttl: 1h
            sliding: true
            routes:
              - method: POST
                path: /v1/charges
                source: inline
        "#;
        let loaded = load_spec_from_str(yaml, SpecFormat::Yaml, None).unwrap();
        let rendered = render(&loaded.spec, SpecFormat::Yaml).unwrap();
        // Re-parse and confirm we land in the same place.
        let again = load_spec_from_str(&rendered, SpecFormat::Yaml, None).unwrap();
        assert_eq!(again.spec.name, "stripe-mock");
        assert_eq!(again.spec.description.as_deref(), Some("Stripe mock"));
        assert_eq!(again.spec.ttl.as_deref(), Some("1h"));
        assert_eq!(again.spec.sliding, Some(true));
        assert_eq!(again.normalized_routes.len(), 1);
        assert_eq!(again.normalized_routes[0].source, "inline");
    }

    #[test]
    fn missing_name_errors() {
        let yaml = r#"
            description: nope
            routes: []
        "#;
        let err = load_spec_from_str(yaml, SpecFormat::Yaml, None).unwrap_err();
        let msg = format!("{err:#}");
        // serde reports "missing field `name`" before we get to the
        // emptiness check — either is acceptable, the spec is rejected.
        assert!(msg.contains("name"), "names the missing field: {msg}");
    }

    #[test]
    fn unknown_field_is_rejected() {
        // deny_unknown_fields catches typos in spec authoring.
        let yaml = r#"
            name: g
            wirremirage: 1  # typo
            routes: []
        "#;
        let err = load_spec_from_str(yaml, SpecFormat::Yaml, None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("wirremirage") || msg.contains("unknown"),
            "names the typo: {msg}"
        );
    }
}
