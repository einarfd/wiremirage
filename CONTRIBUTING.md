# Contributing

Thanks for looking. WireMirage is a personal project run in the open — issues
and pull requests are welcome, with a couple of honest caveats up front:

- It's pre-1.0 and opinionated. Some things are the way they are on purpose;
  the [ADRs](docs/adr/index.md) say which and why. If a change contradicts an
  ADR, that's fine — but argue with the ADR, not around it.
- Review is best-effort and single-maintainer. For anything larger than a bug
  fix, open an issue first so you don't build something that gets declined.

## Getting set up

You need a stable Rust toolchain plus:

```sh
rustup update stable                        # see the note below
rustup target add wasm32-unknown-unknown
cargo install just wasm-tools
```

and **Docker**, which is a build dependency: `crates/wm-host/build.rs` runs
`compiler/js-engine/Dockerfile` to produce the WebAssembly engine component
that gets embedded into the host binary. If Docker isn't available, build the
component elsewhere and point at it with
`WM_JS_ENGINE_WASM_OVERRIDE=/abs/path/to/js-engine.wasm`.

The first build is slow — wasmtime and swc dominate the dependency tree.

`rust-toolchain.toml` selects `stable`, which resolves to whatever stable
you have installed — rustup never moves it for you. CI always gets the
newest stable, so an old local toolchain means `just check` runs a
*weaker* clippy than the gate will, and a lint you never saw fails the PR.
`rustup update stable` is the fix.

Two versions in CI are deliberately not stable:

- **Lints are pinned** (currently 1.98.0). clippy gains lints every
  release, so a floating toolchain would redden CI on a commit that
  changed nothing. Building and testing still run on stable, so a real
  future-compiler breakage is still caught.
- **`rust-version` (currently 1.95) is the supported floor**, checked by
  the `msrv` job. Don't raise it to match your compiler — it's a
  compatibility promise, and it moves only when a dependency forces it.

```sh
just check      # fmt check + clippy -D warnings + tests. The gate.
just build
```

Run it:

```sh
WM_STORAGE=memory WM_BOOTSTRAP_TOKEN=wmt_dev_local \
  WM_BOOTSTRAP_EMAIL=admin@local cargo run -p wm-host
```

`just run-web` boots the host with dev credentials for UI work
(`admin@local` / `devpassword` at `http://localhost:8080/ui/`).

## Before you open a PR

- `just check` passes. CI runs the same thing plus `cargo deny` and the
  Valkey-backed tier-3 suite.
- New behaviour has a test. Most feature work belongs in the tier-2
  integration tests under `crates/wm-host/tests/` — they drive a real host
  in-process over in-memory storage.
- Docs that your change makes wrong are updated in the same PR. That includes
  `docs/`, the shipped skill under `skill/` when the CLI or handler API moves,
  and `README.md`.
- A capability added to one surface should land on the others too (CLI, MCP,
  UI) unless there's a real reason it can't. See
  [AGENTS.md](AGENTS.md#two-rules-that-keep-the-surfaces-honest).
- Significant design decisions want an ADR. Ask in the issue and the
  maintainer will scaffold one.

Commit messages are conventional-ish (`feat(scope): …`, `fix(...)`,
`docs(...)`, `deps: …`). Keep the subject imperative and under ~72 chars.

## Things that are deliberately out of scope

Before proposing these, read the linked ADR — they're settled decisions, not
gaps:

- **Record-and-replay** of real traffic —
  [ADR-0006](docs/adr/0006-recording-separate.md).
- **More handler languages** right now. The engine architecture supports it
  ([ADR-0020](docs/adr/0020-shared-wasm-engine-for-interpreted-languages.md));
  the case for a second language hasn't been made by a real need yet.
- **General network egress from handlers.** Outbound callbacks are a narrow,
  gated exception — [ADR-0034](docs/adr/0034-outbound-callbacks.md).
- **Durable storage / backups.** Everything is ephemeral on purpose.

## Reporting bugs

Include what you'd want if you were fixing it: what you ran, what happened,
what you expected, and the host's log output. If it involves a handler,
include the source. Journal entries (`wm journal show <group>/<n>`) are
usually the fastest evidence — sanitize them first.

Security issues go to [SECURITY.md](SECURITY.md), not the issue tracker.

## Code of conduct

Be decent. The full text is in [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
