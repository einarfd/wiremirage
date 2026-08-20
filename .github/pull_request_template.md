<!-- What changes and why. Link the issue or ADR if there is one. -->

## What

## Why

## Checklist

- [ ] `just check` passes (fmt, clippy `-D warnings`, tests)
- [ ] New behaviour is covered by a test — tier-2 under `crates/wm-host/tests/`
      for anything crossing a surface
- [ ] Docs updated where this makes them wrong (`docs/`, `README.md`, and
      `skill/` when the CLI or handler API changed)
- [ ] Landed on the other surfaces (CLI / MCP / UI) or there's a reason it
      shouldn't
- [ ] ADR added or updated if this is a design decision
