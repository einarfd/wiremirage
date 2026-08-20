# ADR-0006: Recording is a separate tool

**Status:** Accepted

**Context:** WireMock and similar mock servers ship with built-in record-and-replay: configure the server to proxy a real backend, capture the traffic, save it as stub mappings. This is genuinely useful — most people don't want to hand-write mocks for an API they don't fully understand.

For WireMirage, where mocks are scripts rather than declarative documents, the natural recording output would be... what, exactly? Auto-generated TypeScript handlers? Captured request/response pairs that handlers then read?

**Decision:** Recording is not built into the host. It's a separate concern, handled by an external tool (e.g., `mitmproxy`) that produces structured traces. The traces are then handed to an LLM (Claude Code, typically), which generates handler scripts.

**Consequences:**

- **The host stays focused.** No proxy mode, no recording mode, no "record-and-replay vs. mock-from-scratch" mode flag. Handlers are always handler scripts; the host always executes them.
- **Recording quality benefits from LLM mediation.** A captured trace becomes a *starting point* for a handler, not a 1:1 replay. The LLM can produce a handler that captures the *intent* (e.g., "the third call should return rate-limited") rather than just replaying the bytes. This is genuinely better for tests that need to model behavior, not just observed traffic.
- **Recording requires more steps for the user.** "Run mitmproxy in front of your real backend, capture the trace, give it to Claude with a prompt like 'turn this into WireMirage handlers'." Three steps instead of one. Acceptable given the LLM-mediation gain, and the agent does the work anyway.
- **Existing tools cover the recording side.** `mitmproxy` is mature, well-documented, exports flow files, supports HTTPS via its CA. We don't need to build it.
- **No proxy mode in the host.** If a use case appears that genuinely needs in-flight proxying (e.g., "record while testing"), we'd add it as a proxy-mode handler that uses a `wasi:http/outgoing-handler` import — keeping it inside the handler abstraction rather than as a host feature.

**Alternatives considered:**

- **Build a native recording feature into the host.** Considered. Would mean implementing proxy mode, capture-to-storage, "convert capture to handler" logic. Significant feature surface for something existing tools already do well. Rejected.
- **Build a recording-to-handler-script tool that's part of the WireMirage project but separate from the host.** Possible future work. Could be a CLI like `wiremirage record --target https://api.example.com --output handlers/example/`. Reasonable to add later if there's demand. Not v1.
- **Provide a "playback" mode that consumes a flow file directly without converting to handlers.** Tempting for fidelity-critical replay (auth flows where the exact byte sequence matters). Probably not worth the architecture complexity; if needed, a handler can read a captured trace from disk and replay it byte-by-byte.

See also: the architecture overview's "What's deliberately not here" section.
