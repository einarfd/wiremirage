//! `wm-engine-transpile <in.ts> <out.js>` — the module-shape transpile, as a
//! command.
//!
//! `build.rs` calls the library directly; this exists for the one place that
//! cannot, the release image (ADR-0038). Its Dockerfile builds the engine in
//! a node stage that runs before any Rust is compiled, so the TS→JS step has
//! to be something that stage can consume: a binary from an earlier stage.
//!
//! Deliberately not a general-purpose CLI. Two positional paths, module
//! shape, no flags — the moment it grows options, the temptation is to use
//! it somewhere `wm_transpile::transpile_module` would have done.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, output] = args.as_slice() else {
        eprintln!("usage: wm-engine-transpile <input.ts> <output.js>");
        return ExitCode::FAILURE;
    };

    let source = match std::fs::read_to_string(input) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("read {input}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let js = match wm_transpile::transpile_module(&source) {
        Ok(js) => js,
        Err(e) => {
            eprintln!("transpile {input}: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = std::fs::write(output, js) {
        eprintln!("write {output}: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
