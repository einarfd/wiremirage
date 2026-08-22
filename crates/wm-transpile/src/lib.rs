//! TypeScript → JavaScript transpile (ADR-0020 slice B, ADR-0038).
//!
//! Two entry points over one pipeline, differing only in the shape of
//! the output:
//!
//! * [`transpile`] — **script shape**, for user handlers. The engine
//!   evaluates handler source through `new Function(src + "; return
//!   handle;")`, which needs a bare `function handle`, so a leading
//!   `export` is rewritten away.
//! * [`transpile_module`] — **ES module shape**, for the engine's own
//!   `engine.ts`. componentize-js resolves the world's export from a
//!   real `export function handle`, so the export must survive.
//!
//! Both are the same swc pipeline: that is the point (ADR-0038). The
//! engine build exercises the transpiler that user handlers depend on.
//!
//! Pure-Rust path via swc. Lets the host accept `language:
//! "typescript"` routes without an external compiler. swc's `strip`
//! transform erases TS-only syntax (type annotations, interfaces,
//! `as` casts, enums-to-objects); the emitter produces script-shape JS
//! that matches what the shared js-engine.wasm component expects.
//!
//! Why not the engine-baked options. We tried `sucrase` (~50 KB
//! focused transpiler) and `tsc` (~7 MB pure-JS) inside the engine
//! component; both tripped wasm `unreachable` traps inside
//! StarlingMonkey — the former at runtime, the latter at Wizer
//! initialisation. The pure-Rust swc path side-steps the JS-engine
//! layer entirely. Heavier dep tree, but reliable and host-native.

use swc_common::{FileName, GLOBALS, Globals, Mark, SourceMap, sync::Lrc};
use swc_ecma_ast::{EsVersion, Pass, Program};
use swc_ecma_codegen::text_writer::JsWriter;
use swc_ecma_codegen::{Config as CodegenConfig, Emitter};
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};
use swc_ecma_transforms_typescript::strip;

/// Strip TypeScript syntax from `source` and return the JS that
/// remains, **as an ES module** — imports and exports untouched.
/// Returns the swc parse-error message on failure, usually with line
/// and column.
///
/// The function is synchronous and CPU-bound. Callers in async
/// contexts must run it via `tokio::task::spawn_blocking`.
pub fn transpile_module(source: &str) -> Result<String, String> {
    let cm: Lrc<SourceMap> = Lrc::default();
    let fm = cm.new_source_file(FileName::Anon.into(), source.to_string());

    let lexer = Lexer::new(
        Syntax::Typescript(TsSyntax {
            // Decorators + tsx are off by default; we don't need
            // them for handler-shaped TS. Turning them off keeps
            // the parser strict about what the engine will then
            // try to run.
            decorators: false,
            tsx: false,
            ..Default::default()
        }),
        EsVersion::Es2022,
        StringInput::from(&*fm),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let module = parser
        .parse_module()
        .map_err(|e| format!("parse: {:?}", e.into_kind()))?;

    let globals = Globals::default();
    let output = GLOBALS.set(&globals, || -> Result<String, String> {
        let unresolved_mark = Mark::new();
        let top_level_mark = Mark::new();
        // `strip` returns a `Pass` in modern swc. `Pass::process`
        // walks the AST and mutates it in place to remove TS-only
        // syntax. The Program wrapper is what the trait expects;
        // we lift the Module into one, then peel it back after.
        let mut program = Program::Module(module);
        strip(unresolved_mark, top_level_mark).process(&mut program);
        let Program::Module(stripped) = program else {
            return Err("strip lowered Module to Script unexpectedly".into());
        };

        let mut buf = vec![];
        {
            let writer = JsWriter::new(cm.clone(), "\n", &mut buf, None);
            let mut emitter = Emitter {
                cfg: CodegenConfig::default(),
                cm: cm.clone(),
                comments: None,
                wr: writer,
            };
            emitter
                .emit_module(&stripped)
                .map_err(|e| format!("emit: {e}"))?;
        }
        String::from_utf8(buf).map_err(|e| format!("utf8: {e}"))
    })?;

    Ok(output)
}

/// Strip TypeScript syntax and return **script-shape** JS: the same
/// output as [`transpile_module`], with a leading `export` removed from
/// `export function handle`.
///
/// This is the handler path. The engine's eval wrapper looks for a
/// top-level `handle` declaration, so `; return handle;` only resolves
/// against a bare `function handle`. The rewrite is a literal-string
/// swap because the only place `export` appears in the handler contract
/// is in front of `function handle`; an `export const` is user error.
pub fn transpile(source: &str) -> Result<String, String> {
    Ok(transpile_module(source)?.replace("export function handle", "function handle"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_type_annotations() {
        let ts = "type Body = { ok: boolean };\n\
                  function handle(req: unknown): Body { return { ok: true } as Body; }";
        let js = transpile(ts).unwrap();
        assert!(!js.contains("type Body"));
        assert!(!js.contains(": Body"));
        assert!(!js.contains(": unknown"));
        assert!(!js.contains(" as Body"));
        assert!(js.contains("function handle"));
    }

    #[test]
    fn rewrites_export_function_handle() {
        let ts = "export function handle() { return 1; }";
        let js = transpile(ts).unwrap();
        // The literal `export function handle` is gone — the
        // engine's eval wrapper expects a bare `function handle`.
        assert!(!js.contains("export function handle"));
        assert!(js.contains("function handle"));
    }

    #[test]
    fn surfaces_parse_errors_with_message() {
        let bad = "function handle(req: unknown {";
        let err = transpile(bad).unwrap_err();
        assert!(err.contains("parse"), "error mentions parse: {err}");
    }
}

#[cfg(test)]
mod module_shape_tests {
    use super::*;

    #[test]
    fn module_entry_point_keeps_the_export() {
        // What the engine build needs: componentize-js resolves the
        // world's export from this declaration.
        let js = transpile_module("export function handle(req: unknown) { return req; }").unwrap();
        assert!(js.contains("export function handle"), "got: {js}");
    }

    #[test]
    fn handler_entry_point_strips_it() {
        // What the engine's eval wrapper needs.
        let js = transpile("export function handle(req: unknown) { return req; }").unwrap();
        assert!(js.contains("function handle"));
        assert!(!js.contains("export function handle"), "got: {js}");
    }

    #[test]
    fn both_entry_points_strip_the_same_types() {
        let ts = "interface R { a: number }\nexport function handle(r: R): number { return r.a; }";
        let (m, s) = (transpile_module(ts).unwrap(), transpile(ts).unwrap());
        for js in [&m, &s] {
            assert!(!js.contains("interface R"));
            assert!(!js.contains(": number"));
        }
        assert_eq!(m.replace("export function handle", "function handle"), s);
    }

    #[test]
    fn module_entry_point_keeps_imports() {
        // engine.ts imports the WIT interfaces; componentize-js needs them.
        let js = transpile_module("import { x } from 'a:b/c@0.1.0';\nexport const y = x;").unwrap();
        assert!(js.contains("import"), "got: {js}");
    }
}
