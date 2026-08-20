# ADR-0015: CLI plus skill as the primary agent surface; MCP as secondary

**Status:** Accepted

**Context:** Earlier ADRs ([0009-html-htmx-ui.md](0009-html-htmx-ui.md), [0012-api-tokens.md](0012-api-tokens.md)) committed to an MCP server as the primary surface for agent-driven interactions. The reasoning at the time was that MCP was the canonical way to expose programmatic capabilities to AI coding agents.

The landscape shifted in late 2025 / early 2026:

- **Anthropic Skills** (Oct 2025) introduced a markdown-and-scripts model that's discoverable from the filesystem, doesn't require running services, and gives agents structured workflow guidance rather than just tool surfaces.
- **CLI tools paired with the agent's Bash tool** turned out to compose better with the rest of a developer's environment than dedicated MCP servers. Make, CI, pre-commit hooks, scripts in other languages — they all integrate with a CLI; only MCP-aware clients integrate with an MCP server.
- **Debugging CLI breakage is significantly easier** than debugging an MCP integration. Run the command in a terminal, read the error. No protocol layer, no tool-schema layer, no transport.
- **Schema authoring is a real cost** for MCP servers and a non-cost for CLIs. CLI conventions (`--help`, `--json`, exit codes, stderr) are things agents already know how to use.
- **OSS positioning improves with a CLI-first framing.** "WireMirage is a mock server with a CLI" reads as a complete tool to any developer; "a mock server with an MCP server" reads as AI-specific to many evaluators.

Four places where MCP still earns its keep specifically:

1. **Streaming events** — the live journal tail benefits from SSE-shaped delivery; expressing "wait for events" through a blocking CLI command is awkward for agents.
2. **Remote-without-install** — a user whose WireMirage runs in a place they can't easily install a CLI binary (CI, a colleague's K8s cluster) can still reach an MCP endpoint by URL with a bearer token.
3. **Agents without CLI access** — sandboxed agent environments (e.g., Anthropic's hosted Claude with MCP servers but no Bash tool), corporate environments where arbitrary command execution is restricted, and other deployment contexts where MCP is available but a CLI binary isn't installable. These agents need to drive WireMirage end-to-end via MCP, which is fine — MCP exposes the operations they need.
4. **Native client integration** — Claude Desktop, Cursor, and similar tools surface MCP servers in their UI, which has real ergonomic value for some users.

**Decision:** Ship both, with clear roles.

- **CLI tool (`wm`)** is the **primary** programmatic surface. A single binary distributed via brew/cargo/apt, calling the REST API. Authenticates with the same API tokens defined in [0012-api-tokens.md](0012-api-tokens.md). Lives in the same repository as the host as a Cargo workspace member.
- **WireMirage skill** is the **primary** agent integration. A `SKILL.md` plus a `scripts/` directory teaching agents when to reach for WireMirage, the common workflow patterns, and ready-made shell scripts for typical setups. Distributed as part of the WireMirage release.
- **MCP server** is the **secondary** surface, specifically for streaming the live journal and for remote access without local install. Same Cargo workspace, same authentication, called by agents that can talk MCP and need streaming or are running somewhere a CLI install isn't practical.

The host implements the operations once internally. The REST API exposes them. The CLI calls the REST API. The MCP server calls the same internal functions and adds the streaming primitives that REST handles via SSE.

**Consequences:**

- **CLI-first framing helps adoption.** Developers evaluating WireMirage encounter a familiar shape: install the CLI, point it at the server, run commands. The agent integration is additive on top of that, not the framing.
- **Skill captures workflow knowledge that MCP can't.** "When to use this tool, what the patterns look like, what gotchas to watch for" lives in `SKILL.md`. MCP exposes tools but not workflows; skills are workflow-shaped.
- **Composition with the rest of the dev environment is free.** `wm` works in Bash scripts, Make targets, CI pipelines, pre-commit hooks, and any other context — same as `git`, `kubectl`, `gh`. MCP servers compose only with MCP-aware clients.
- **Debugging is easier.** When an agent's mock setup fails, the developer can rerun the failing `wm` command in a terminal and see exactly what happened. No protocol-layer mysteries.
- **Three surfaces need maintenance instead of one.** REST API, CLI, MCP. Mitigated by the CLI and MCP both being thin shims over the REST API; the actual logic lives in one place.
- **Streaming use cases route through MCP, not CLI.** A `wm journal tail` command is provided for human use, but agents wanting to "wait until my SUT hits the mock" should use the MCP streaming tool — the agent's Bash tool doesn't handle long-running blocking commands well.
- **MCP is a parallel path to the same operations, not a strict subset.** Agents with no CLI access need to drive WireMirage end-to-end via MCP; the MCP surface includes group/route CRUD and state management for that reason, not just the streaming primitives. MCP omits things outside the agent's normal workflow (token management, user management, session management) but covers the rest. See ../mcp-surface.md.
- **Distribution gains a binary.** The host runs in a container; the CLI is a binary that runs on developer machines and CI runners. Different distribution channels, but Cargo's `cargo install` handles the developer-machine case cleanly.

**Alternatives considered:**

- **MCP-only (the original v1 proposal).** Considered. Rejected because the landscape shift makes it the wrong default in 2026. The case for MCP as the primary agent surface was strongest before skills and CLI-via-Bash matured; that case has eroded.
- **CLI-only, no MCP.** Considered. Rejected because the streaming-journal use case is genuinely better served by MCP, and the remote-access case is real. Adding MCP costs little once the REST API and CLI exist.
- **CLI-only, with MCP added later if asked.** Considered seriously. Rejected because MCP-shaped streaming gives the agent a meaningfully better journal-tail experience than CLI piping; we can do this now while the architecture is fluid.
- **Skill that wraps MCP instead of CLI.** A skill could conceivably document MCP usage rather than CLI usage. Rejected because the skill's main value is in workflow guidance, and `wm` commands compose with shell scripts in `scripts/` more naturally than MCP tool calls.
- **Separate repository for the CLI.** Cleaner versioning, smaller dependency surface. Rejected for v1 — keeping the CLI in-tree as a Cargo workspace member means contributors fix bugs in lockstep, releases ship together, and we can split later if it becomes desirable. Easy to extract; hard to merge.

**Implementation shape:**

```
wiremirage/                       (Cargo workspace root)
├── Cargo.toml                    (workspace manifest)
├── crates/
│   ├── wm-host/                  (the Rust host binary)
│   ├── wm-cli/                   (the CLI binary)
│   ├── wm-mcp/                   (the MCP server)
│   └── wm-core/                  (shared types, REST client, auth)
└── skill/
    ├── wiremirage/               (main skill; describes when and how to use WireMirage)
    │   ├── SKILL.md
    │   └── scripts/
    │       ├── setup-stripe-mock.sh
    │       ├── reset-state.sh
    │       └── ...
    └── wiremirage-debug/         (sub-skill; loads when troubleshooting a mock)
        └── SKILL.md
```

`wm-core` holds the shared types and REST client used by both `wm-cli` and `wm-mcp`. The CLI and MCP both authenticate against the host's REST API using the same token mechanism.

The CLI, host, and MCP server all live in the same repository. We'll re-evaluate that decision if and only if it becomes problematic in practice — the cost of splitting later is small relative to the friction of working across two repos from day one.

`wm-mcp` is shown above as a separate crate. Whether it's actually a separate crate, a feature flag of `wm-host`, or a binary built from `wm-host` source is an implementation question to settle when the code starts. The decision-shape for v1 is "the MCP server exists and ships in this repo"; the exact crate boundary is below the level of design.

See also: ../cli-design.md, ../skill-design.md, [0012-api-tokens.md](0012-api-tokens.md), [0011-route-ownership.md](0011-route-ownership.md), ../auth-and-authz.md.
