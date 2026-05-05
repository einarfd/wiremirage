// Build script for wm-host.
//
// At build time we compile the standalone fixture guests under
// tests/fixtures/ to wasm32-unknown-unknown, run `wasm-tools component new`
// to wrap each module as a Component, and stamp the resulting paths into
// `cargo:rustc-env` so tier-2 integration tests can locate them via
// `env!("WM_FIXTURE_<name>_COMPONENT")`.
//
// The fixtures are self-contained crates (their own Cargo.toml + lockfile)
// and live outside the workspace, so this `cargo build` does not recurse
// into the parent workspace.
//
// Required tooling on PATH:
//   - cargo with `wasm32-unknown-unknown` target installed
//   - wasm-tools (https://github.com/bytecodealliance/wasm-tools)

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

    let fixtures = ["echo-handler"];
    for name in fixtures {
        build_fixture(&manifest_dir, &out_dir, name);
    }
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

    let status = Command::new("cargo")
        .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
        .current_dir(&fixture_dir)
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
