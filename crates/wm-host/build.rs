// Build script for wm-host.
//
// At build time we:
//   1. Compile the standalone fixture guests under tests/fixtures/ to
//      wasm32-unknown-unknown, run `wasm-tools component new` to wrap each
//      module as a Component, and stamp the resulting paths into
//      `cargo:rustc-env` so tier-2 integration tests can locate them via
//      `env!("WM_FIXTURE_<name>_COMPONENT")`.
//   2. Build the shared js-engine.wasm component (ADR-0020) by running
//      `compiler/js-engine/`'s Dockerfile-based build, dropping the
//      output into OUT_DIR, and stamping the path into
//      `cargo:rustc-env=WM_JS_ENGINE_WASM=...` for main.rs to
//      `include_bytes!`.
//
// The fixtures are self-contained crates (their own Cargo.toml + lockfile)
// and live outside the workspace, so this `cargo build` does not recurse
// into the parent workspace.
//
// Required tooling on PATH:
//   - cargo with `wasm32-unknown-unknown` target installed
//   - wasm-tools (https://github.com/bytecodealliance/wasm-tools)
//   - docker (for the js-engine build; set
//     WM_JS_ENGINE_WASM_OVERRIDE=/path/to/prebuilt.wasm to skip)

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Re-run this script when the WIT contract or fixture sources change.
    // (Cargo will also re-run on its own dependency changes.)
    println!(
        "cargo:rerun-if-changed={}",
        workspace_root.join("wit/wiremirage.wit").display()
    );

    let fixtures = ["echo-handler", "counter-handler"];
    for name in fixtures {
        build_fixture(&manifest_dir, &out_dir, name);
    }

    build_js_engine(workspace_root, &out_dir);
}

fn build_fixture(manifest_dir: &Path, out_dir: &Path, name: &str) {
    let fixture_dir = manifest_dir.join("tests/fixtures").join(name);

    println!(
        "cargo:rerun-if-changed={}",
        fixture_dir.join("src/lib.rs").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        fixture_dir.join("Cargo.toml").display()
    );

    // Fixture crates are standalone (their own Cargo.toml + lockfile)
    // and live outside the workspace. When this build.rs runs under
    // `cargo clippy`, cargo sets `RUSTC_WORKSPACE_WRAPPER=clippy-driver`
    // in the env; that propagates here, and our nested `cargo build`
    // would inherit it and run clippy on the fixture too. The fixture's
    // `wit_bindgen::generate!` macro expands to code that trips
    // `clippy::too_many_arguments`, which we can't fix. Force the
    // nested cargo back to a plain rustc by clearing the wrapper env
    // vars before spawning.
    let status = Command::new("cargo")
        .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
        .current_dir(&fixture_dir)
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC")
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "fixture {name}: failed to spawn cargo build: {e} \
                 (is the wasm32-unknown-unknown target installed? \
                 `rustup target add wasm32-unknown-unknown`)"
            )
        });
    assert!(status.success(), "fixture {name}: cargo build failed");

    let module_wasm = fixture_dir
        .join("target/wasm32-unknown-unknown/release")
        .join(name.replace('-', "_"))
        .with_extension("wasm");
    let component_wasm = out_dir.join(format!("{}.component.wasm", name.replace('-', "_")));

    let status = Command::new("wasm-tools")
        .args([
            "component",
            "new",
            module_wasm.to_str().unwrap(),
            "-o",
            component_wasm.to_str().unwrap(),
        ])
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "fixture {name}: failed to spawn wasm-tools: {e} \
                 (install via `cargo install wasm-tools`)"
            )
        });
    assert!(
        status.success(),
        "fixture {name}: wasm-tools component new failed"
    );

    let env_name = format!(
        "WM_FIXTURE_{}_COMPONENT",
        name.replace('-', "_").to_uppercase()
    );
    println!("cargo:rustc-env={}={}", env_name, component_wasm.display());
}

/// Build the shared js-engine.wasm component (ADR-0020).
///
/// Uses `compiler/js-engine/Dockerfile` so contributors only need
/// Docker on PATH — not Node / npm / componentize-js. Docker layer
/// caching makes repeated builds <1 s when nothing changed.
///
/// `WM_JS_ENGINE_WASM_OVERRIDE=/abs/path/to/prebuilt.wasm` skips the
/// docker invocation entirely and uses the supplied artifact. Useful
/// for: end-users building from source without Docker (download a
/// release artifact), the release-image build itself (single-stage
/// Dockerfile pre-builds the engine and passes it in), and constrained
/// CI environments.
fn build_js_engine(workspace_root: &Path, out_dir: &Path) {
    let engine_dir = workspace_root.join("compiler/js-engine");
    let out_wasm = out_dir.join("js-engine.wasm");

    // Re-run triggers. Cargo only invokes build.rs when at least one of
    // these changes (or the script itself changes); on warm rebuilds the
    // docker invocation below isn't reached.
    println!("cargo:rerun-if-env-changed=WM_JS_ENGINE_WASM_OVERRIDE");
    for p in [
        "src/engine.ts",
        "wit",
        "package.json",
        "package-lock.json",
        "build.mjs",
        "Dockerfile",
        ".dockerignore",
    ] {
        println!("cargo:rerun-if-changed={}", engine_dir.join(p).display());
    }

    if let Ok(override_path) = env::var("WM_JS_ENGINE_WASM_OVERRIDE") {
        std::fs::create_dir_all(out_dir).expect("create OUT_DIR");
        std::fs::copy(&override_path, &out_wasm).unwrap_or_else(|e| {
            panic!(
                "copy WM_JS_ENGINE_WASM_OVERRIDE={override_path} to {}: {e}",
                out_wasm.display()
            )
        });
        println!("cargo:rustc-env=WM_JS_ENGINE_WASM={}", out_wasm.display());
        return;
    }

    let image_tag = "wm-js-engine-builder:dev";

    let status = Command::new("docker")
        .args(["build", "--quiet", "-t", image_tag, "."])
        .current_dir(&engine_dir)
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "spawn `docker build` for js-engine: {e}\n\n\
                 Docker is required to build wm-host (ADR-0020). Either:\n\
                   - install Docker, or\n\
                   - set WM_JS_ENGINE_WASM_OVERRIDE=/abs/path/to/prebuilt.wasm"
            )
        });
    assert!(status.success(), "docker build of js-engine image failed");

    let uid = read_id("id", &["-u"]);
    let gid = read_id("id", &["-g"]);

    std::fs::create_dir_all(out_dir).expect("create OUT_DIR");
    let status = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--user",
            &format!("{uid}:{gid}"),
            "-v",
            &format!("{}:/out", out_dir.display()),
            "-e",
            "WM_JS_ENGINE_OUT=/out/js-engine.wasm",
            image_tag,
        ])
        .status()
        .expect("spawn `docker run` for js-engine build");
    assert!(status.success(), "docker run of js-engine build failed");

    println!("cargo:rustc-env=WM_JS_ENGINE_WASM={}", out_wasm.display());
}

fn read_id(cmd: &str, args: &[&str]) -> String {
    let output = Command::new(cmd)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn `{cmd} {}`: {e}", args.join(" ")));
    String::from_utf8(output.stdout)
        .expect("uid/gid output is utf-8")
        .trim()
        .to_string()
}
