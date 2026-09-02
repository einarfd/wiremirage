//! TypeScript → JavaScript transpile (ADR-0020 slice B, ADR-0038).
//!
//! Two entry points over one pipeline, differing only in the shape of
//! the output:
//!
//! * [`transpile`] — **script shape**, for user handlers. The engine
//!   evaluates handler source through `new Function(src + "; return
//!   handle;")`, which is a *script*: a surviving top-level `export`
//!   or `import` is a syntax error there. So the module's exports are
//!   unwrapped into plain declarations, and the forms that can't be
//!   unwrapped are rejected here rather than at request time.
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
//! `language: "javascript"` goes through the same path — plain JS is
//! valid TypeScript input, so both languages get the same export
//! handling and the same create-time validation.
//!
//! Why not the engine-baked options. We tried `sucrase` (~50 KB
//! focused transpiler) and `tsc` (~7 MB pure-JS) inside the engine
//! component; both tripped wasm `unreachable` traps inside
//! StarlingMonkey — the former at runtime, the latter at Wizer
//! initialisation. The pure-Rust swc path side-steps the JS-engine
//! layer entirely. Heavier dep tree, but reliable and host-native.

use swc_common::{FileName, GLOBALS, Globals, Mark, SourceMap, Span, sync::Lrc};
use swc_ecma_ast::{
    Decl, DefaultDecl, EsVersion, FnDecl, Module, ModuleDecl, ModuleItem, ObjectPatProp, Pass, Pat,
    Program, Script, Stmt,
};
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
    let (cm, module) = parse_and_strip(source)?;
    emit(&cm, &Program::Module(module))
}

/// Strip TypeScript syntax and return **script-shape** JS: the module's
/// exports unwrapped into plain top-level declarations, ready for the
/// engine's `new Function(src + "; return handle;")` wrapper.
///
/// This is the handler path. `new Function` builds a *script*, where a
/// top-level `export` or `import` is a syntax error — so every export
/// form the contract accepts has to be rewritten away here rather than
/// reaching the engine. Erroring instead of emitting something that
/// cannot run is what makes `compile_failed` mean what it says: this
/// returns `Err` for the forms that would 500 at request time, so the
/// caller sees them at create time.
pub fn transpile(source: &str) -> Result<String, String> {
    let (cm, module) = parse_and_strip(source)?;
    let script = to_script_shape(&cm, module)?;
    emit(&cm, &Program::Script(script))
}

/// Parse as TypeScript and run swc's `strip` pass. Shared by both entry
/// points — that sameness is the point of ADR-0038.
fn parse_and_strip(source: &str) -> Result<(Lrc<SourceMap>, Module), String> {
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
    let stripped = GLOBALS.set(&globals, || -> Result<Module, String> {
        let unresolved_mark = Mark::new();
        let top_level_mark = Mark::new();
        // `strip` returns a `Pass` in modern swc. `Pass::process`
        // walks the AST and mutates it in place to remove TS-only
        // syntax. The Program wrapper is what the trait expects;
        // we lift the Module into one, then peel it back after.
        let mut program = Program::Module(module);
        strip(unresolved_mark, top_level_mark).process(&mut program);
        match program {
            Program::Module(m) => Ok(m),
            Program::Script(_) => Err("strip lowered Module to Script unexpectedly".into()),
        }
    })?;

    Ok((cm, stripped))
}

fn emit(cm: &Lrc<SourceMap>, program: &Program) -> Result<String, String> {
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
            .emit_program(program)
            .map_err(|e| format!("emit: {e}"))?;
    }
    String::from_utf8(buf).map_err(|e| format!("utf8: {e}"))
}

/// Rewrite a handler module into an equivalent script.
///
/// The rule is that every accepted form names `handle` in the source,
/// so unwrapping it is a rewrite rather than an inference. The moment a
/// form would require *deciding* that something unnamed is the handler
/// — an anonymous `export default` — we stop and say so. Imports and
/// re-exports are rejected for a harder reason: a script has no module
/// loader to resolve them against.
fn to_script_shape(cm: &Lrc<SourceMap>, module: Module) -> Result<Script, String> {
    let mut body = Vec::with_capacity(module.body.len());
    for item in module.body {
        match item {
            ModuleItem::Stmt(stmt) => body.push(stmt),
            ModuleItem::ModuleDecl(decl) => {
                if let Some(stmt) = unwrap_export(cm, decl)? {
                    body.push(stmt);
                }
            }
        }
    }
    let script = Script {
        span: module.span,
        body,
        shebang: module.shebang,
    };
    if !declares_handle(&script) {
        return Err(
            "handler source must declare a top-level `handle` function — \
                    e.g. `export function handle(req, routeStore, groupStore) { ... }`"
                .into(),
        );
    }
    Ok(script)
}

fn unwrap_export(cm: &Lrc<SourceMap>, decl: ModuleDecl) -> Result<Option<Stmt>, String> {
    match decl {
        // `export function handle() {}`, `export const handle = ...`,
        // `export class Handle {}` — drop the keyword, keep the
        // declaration exactly as written.
        ModuleDecl::ExportDecl(e) => Ok(Some(Stmt::Decl(e.decl))),
        // `export { handle }` — the binding is already declared
        // elsewhere in the file, so the statement carries nothing the
        // script needs. A renaming re-export (`export { h as handle }`)
        // is dropped too, and then fails the `handle` check below with
        // a message that says so, rather than being silently aliased.
        ModuleDecl::ExportNamed(n) if n.src.is_none() => Ok(None),
        // `export default function handle() {}` — still names `handle`,
        // so this is an unwrap, not a guess.
        ModuleDecl::ExportDefaultDecl(d) => {
            let span = d.span;
            match d.decl {
                DefaultDecl::Fn(f) => match f.ident {
                    Some(ident) => Ok(Some(Stmt::Decl(Decl::Fn(FnDecl {
                        ident,
                        declare: false,
                        function: f.function,
                    })))),
                    None => Err(at(cm, span, ANONYMOUS_DEFAULT)),
                },
                DefaultDecl::Class(c) => match c.ident {
                    Some(ident) => Ok(Some(Stmt::Decl(Decl::Class(swc_ecma_ast::ClassDecl {
                        ident,
                        declare: false,
                        class: c.class,
                    })))),
                    None => Err(at(cm, span, ANONYMOUS_DEFAULT)),
                },
                DefaultDecl::TsInterfaceDecl(i) => Err(at(cm, i.span, ANONYMOUS_DEFAULT)),
            }
        }
        // `export default handle;` / `export default () => {}`.
        ModuleDecl::ExportDefaultExpr(e) => Err(at(cm, e.span, ANONYMOUS_DEFAULT)),
        ModuleDecl::Import(i) => Err(at(cm, i.span, NO_MODULE_LOADER)),
        ModuleDecl::ExportAll(e) => Err(at(cm, e.span, NO_MODULE_LOADER)),
        // `export { x } from "..."` — the `src.is_none()` arm above
        // took the local form, so this one re-exports from a module.
        ModuleDecl::ExportNamed(n) => Err(at(cm, n.span, NO_MODULE_LOADER)),
        ModuleDecl::TsImportEquals(d) => Err(at(cm, d.span, NO_MODULE_LOADER)),
        ModuleDecl::TsExportAssignment(d) => Err(at(cm, d.span, ANONYMOUS_DEFAULT)),
        ModuleDecl::TsNamespaceExport(d) => Err(at(cm, d.span, NO_MODULE_LOADER)),
    }
}

const ANONYMOUS_DEFAULT: &str = "`export default` without a name doesn't declare `handle` — \
     write `export function handle(...)` or `export default function handle(...)`";

const NO_MODULE_LOADER: &str = "`import` / `export ... from` is not supported: handler source runs as a self-contained \
     script with no module loader. Inline what the handler needs.";

fn at(cm: &Lrc<SourceMap>, span: Span, message: &str) -> String {
    format!("line {}: {message}", cm.lookup_char_pos(span.lo).line)
}

/// Does the script bind the name the engine's eval wrapper looks for?
///
/// The engine's `; return handle;` resolves against any top-level
/// binding, so a `const handle = ...` counts as much as a
/// `function handle`. Checking here turns the engine's runtime
/// "did not declare a top-level `handle` function" into a
/// `compile_failed` at create time.
fn declares_handle(script: &Script) -> bool {
    let mut names = Vec::new();
    for stmt in &script.body {
        let Stmt::Decl(decl) = stmt else { continue };
        match decl {
            Decl::Fn(f) => names.push(f.ident.sym.to_string()),
            Decl::Class(c) => names.push(c.ident.sym.to_string()),
            Decl::Var(v) => {
                for d in &v.decls {
                    pattern_bindings(&d.name, &mut names);
                }
            }
            _ => {}
        }
    }
    names.iter().any(|n| n == "handle")
}

fn pattern_bindings(pat: &Pat, out: &mut Vec<String>) {
    match pat {
        Pat::Ident(i) => out.push(i.id.sym.to_string()),
        Pat::Array(a) => a
            .elems
            .iter()
            .flatten()
            .for_each(|p| pattern_bindings(p, out)),
        Pat::Object(o) => {
            for prop in &o.props {
                match prop {
                    ObjectPatProp::KeyValue(kv) => pattern_bindings(&kv.value, out),
                    ObjectPatProp::Assign(a) => out.push(a.key.sym.to_string()),
                    ObjectPatProp::Rest(r) => pattern_bindings(&r.arg, out),
                }
            }
        }
        Pat::Rest(r) => pattern_bindings(&r.arg, out),
        Pat::Assign(a) => pattern_bindings(&a.left, out),
        _ => {}
    }
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
    fn unwraps_every_export_form_that_names_handle() {
        // Each of these names `handle` in the source, so unwrapping is
        // a rewrite rather than a guess. All of them used to survive as
        // a top-level `export` and 500 at request time.
        for src in [
            "export function handle() { return 1; }",
            "export async function handle() { return 1; }",
            "export const handle = () => 1;",
            "function handle() { return 1; }\nexport { handle };",
            "export default function handle() { return 1; }",
            "function handle() { return 1; }",
        ] {
            let js = transpile(src).unwrap_or_else(|e| panic!("{src:?} rejected: {e}"));
            assert!(
                !js.contains("export"),
                "{src:?} left an export behind, which `new Function` rejects: {js}"
            );
            assert!(js.contains("handle"), "{src:?} lost the binding: {js}");
        }
    }

    #[test]
    fn rejects_anonymous_default_export_rather_than_guessing() {
        for src in [
            "export default function () { return 1; }",
            "export default () => 1;",
        ] {
            let err = transpile(src).unwrap_err();
            assert!(
                err.contains("export default"),
                "{src:?} should name the problem: {err}"
            );
        }
    }

    #[test]
    fn rejects_imports_because_a_script_has_no_module_loader() {
        let err = transpile("import { x } from 'y';\nfunction handle() { return x; }").unwrap_err();
        assert!(err.contains("module loader"), "got: {err}");
        let err = transpile("function handle() {}\nexport { handle } from 'y';").unwrap_err();
        assert!(err.contains("module loader"), "got: {err}");
    }

    #[test]
    fn rejects_source_that_never_declares_handle() {
        // The engine would answer this at request time with "did not
        // declare a top-level `handle` function". Catching it here is
        // what makes create-time validation predictive.
        let err = transpile("function other() { return 1; }").unwrap_err();
        assert!(err.contains("`handle`"), "got: {err}");
        // A renaming re-export is deliberately not aliased — it lands
        // here instead, which is a clearer answer than silence.
        let err = transpile("function h() { return 1; }\nexport { h as handle };").unwrap_err();
        assert!(err.contains("`handle`"), "got: {err}");
    }

    #[test]
    fn accepts_handle_bound_by_any_top_level_declaration() {
        // The engine's `; return handle;` resolves against any binding,
        // so the check has to be about the name, not the syntax.
        for src in [
            "const handle = (req) => ({ status: 200 });",
            "let handle;\nhandle = function () {};",
            "var { handle } = globalThis;",
        ] {
            transpile(src).unwrap_or_else(|e| panic!("{src:?} rejected: {e}"));
        }
    }

    #[test]
    fn error_messages_carry_a_line_number() {
        // The import has to be *used*: swc's strip pass elides an
        // unused one, which is TypeScript's own import-elision rule.
        let err =
            transpile("function handle() { return x; }\n\nimport { x } from 'y';").unwrap_err();
        assert!(err.starts_with("line 3:"), "got: {err}");
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
    fn plain_javascript_goes_through_the_same_path() {
        // `language: "javascript"` reuses this entry point — plain JS is
        // valid TypeScript input, so there is no second pipeline to keep
        // in step and JS gets the same create-time validation.
        let js = transpile("export function handle(req) { return req; }").unwrap();
        assert!(js.contains("function handle"), "got: {js}");
        assert!(transpile("export function handle( {").is_err());
    }

    #[test]
    fn both_entry_points_strip_the_same_types() {
        let ts = "interface R { a: number }\nexport function handle(r: R): number { return r.a; }";
        let (m, s) = (transpile_module(ts).unwrap(), transpile(ts).unwrap());
        for js in [&m, &s] {
            assert!(!js.contains("interface R"));
            assert!(!js.contains(": number"));
        }
        // Same pipeline, differing only in whether the export survives.
        assert!(m.contains("export function handle"));
        assert!(s.contains("function handle") && !s.contains("export"));
    }

    #[test]
    fn module_entry_point_keeps_imports() {
        // engine.ts imports the WIT interfaces; componentize-js needs them.
        let js = transpile_module("import { x } from 'a:b/c@0.1.0';\nexport const y = x;").unwrap();
        assert!(js.contains("import"), "got: {js}");
    }
}

#[cfg(test)]
mod shipped_example {
    /// The example handler we ship to users has to survive the pipeline
    /// its readers will put it through. It uses `import type`, which
    /// TypeScript's import elision removes — so the script-shape output
    /// has no module construct left in it.
    #[test]
    fn example_handler_transpiles_to_script_shape() {
        let src = include_str!("../../../types/example-handler.ts");
        let js = super::transpile(src).expect("shipped example must transpile");
        assert!(!js.contains("export"), "left an export behind: {js}");
        assert!(!js.contains("import"), "left an import behind: {js}");
        assert!(js.contains("function handle"));
    }
}
