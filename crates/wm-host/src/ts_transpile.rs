//! In-host TypeScript → JavaScript transpile (ADR-0020 slice B).
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
/// remains. Returns the swc parse-error message on failure —
/// usually with line + column.
///
/// The function is synchronous and CPU-bound. Callers in async
/// contexts must run it via `tokio::task::spawn_blocking`.
pub fn transpile(source: &str) -> Result<String, String> {
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

    // The engine's `handle` eval wrapper looks for a top-level
    // `handle` declaration. TS source that wrote `export function
    // handle(...)` becomes `export function handle(...)` after
    // strip (the ES module export survives). We patch that to a
    // plain `function handle(...)` so the wrapper's
    // `; return handle;` works. The patching is a literal-string
    // swap because the only place `export` appears in our handler
    // contract is in front of `function handle`; if someone
    // sneaks an `export const`, that's user error.
    let patched = output.replace("export function handle", "function handle");
    Ok(patched)
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
