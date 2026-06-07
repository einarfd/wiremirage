//! CLI file-I/O wrapper over the shared [`wm_core::spec`] format.
//!
//! The format types, normalization, parse, render, and duration parsing all
//! live in `wm_core::spec` so every surface (CLI / REST / MCP / UI) agrees on
//! the shape. The only CLI-specific concern is the filesystem: reading the
//! spec file, detecting the format from its extension, and resolving each
//! route's `source_file` (relative to the spec file's directory) into inline
//! `source` before handing off to the shared normalizer. Reading from stdin
//! (`--from-file -`, `base_dir = None`) forbids `source_file` — there's no
//! directory to be relative to.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use wm_core::spec as core;

pub use wm_core::spec::{GroupSpec, NormalizedRoute, RouteSpec, SpecFormat, render};

/// A spec loaded from disk/stdin: the raw `spec`, plus its routes normalized
/// (source_file resolved to inline source, methods folded, validated) and the
/// parsed TTL in seconds.
#[derive(Debug)]
pub struct LoadedSpec {
    pub spec: GroupSpec,
    pub normalized_routes: Vec<NormalizedRoute>,
    pub ttl_seconds: Option<u64>,
}

/// Read and normalize a spec file. Format is detected from the extension.
pub fn load_spec_from_path(path: &Path) -> Result<LoadedSpec> {
    let format = SpecFormat::from_extension(path).ok_or_else(|| {
        anyhow!(
            "spec file {} has no recognised extension; expected .yaml, .yml, or .json (or use `--from-file -` to read stdin with `--format`)",
            path.display()
        )
    })?;
    let text =
        fs::read_to_string(path).with_context(|| format!("read spec file {}", path.display()))?;
    finish(&text, format, path.parent())
}

/// Read and normalize a spec from a raw string (the stdin path). `base_dir =
/// None` forbids `source_file` references.
pub fn load_spec_from_str(
    text: &str,
    format: SpecFormat,
    base_dir: Option<&Path>,
) -> Result<LoadedSpec> {
    finish(text, format, base_dir)
}

fn finish(text: &str, format: SpecFormat, base_dir: Option<&Path>) -> Result<LoadedSpec> {
    let mut spec = core::parse_str(text, format)?;
    resolve_source_files(&mut spec, base_dir)?;
    let normalized = core::normalize(&spec)?;
    Ok(LoadedSpec {
        spec,
        normalized_routes: normalized.routes,
        ttl_seconds: normalized.ttl_seconds,
    })
}

/// Resolve each route's `source_file` into inline `source`, relative to the
/// spec file's directory. Only touches routes that have a `source_file` and
/// no inline `source` (the both-set case is left for the shared normalizer to
/// reject). Without a `base_dir` (stdin), a `source_file` is an error.
fn resolve_source_files(spec: &mut GroupSpec, base_dir: Option<&Path>) -> Result<()> {
    for (idx, r) in spec.routes.iter_mut().enumerate() {
        if r.source.is_none()
            && let Some(rel) = r.source_file.clone()
        {
            let Some(base) = base_dir else {
                return Err(anyhow!(
                    "route #{idx} ({path}): `source_file` is forbidden when the spec is read from stdin; use inline `source` instead",
                    path = r.path
                ));
            };
            let resolved = base.join(&rel);
            let contents = fs::read_to_string(&resolved).with_context(|| {
                format!(
                    "route #{idx} ({path}): read source_file {}",
                    resolved.display(),
                    path = r.path
                )
            })?;
            r.source = Some(contents);
            r.source_file = None;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The format/normalize logic is unit-tested in `wm_core::spec`; these
    // cover the CLI's file-I/O wrapper (delegation + source_file/stdin).

    #[test]
    fn delegates_parse_and_normalize() {
        let yaml = "name: g\nroutes:\n  - method: POST\n    path: /x\n    source: h\n";
        let loaded = load_spec_from_str(yaml, SpecFormat::Yaml, None).unwrap();
        assert_eq!(loaded.spec.name, "g");
        assert_eq!(loaded.normalized_routes.len(), 1);
        assert_eq!(
            loaded.normalized_routes[0].methods,
            vec!["POST".to_string()]
        );
    }

    #[test]
    fn ttl_is_parsed() {
        let yaml = "name: g\nttl: 24h\nroutes: []\n";
        assert_eq!(
            load_spec_from_str(yaml, SpecFormat::Yaml, None)
                .unwrap()
                .ttl_seconds,
            Some(86_400)
        );
    }

    #[test]
    fn stdin_input_forbids_source_file() {
        let yaml =
            "name: g\nroutes:\n  - method: POST\n    path: /x\n    source_file: ./handler.ts\n";
        let err = load_spec_from_str(yaml, SpecFormat::Yaml, None).unwrap_err();
        assert!(format!("{err:#}").contains("stdin"), "explains it: {err:#}");
    }

    #[test]
    fn source_file_resolves_relative_to_base_dir() {
        let dir = std::env::temp_dir().join(format!("wmspec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("h.ts"), "export const x = 1;").unwrap();
        let yaml = "name: g\nroutes:\n  - method: POST\n    path: /x\n    source_file: ./h.ts\n";
        let loaded = load_spec_from_str(yaml, SpecFormat::Yaml, Some(&dir)).unwrap();
        assert_eq!(loaded.normalized_routes[0].source, "export const x = 1;");
        std::fs::remove_dir_all(&dir).ok();
    }
}
