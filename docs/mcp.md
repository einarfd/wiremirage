# The MCP server

The host exposes an MCP (Model Context Protocol) service at **`/api/mcp`**
over the streamable-HTTP transport. It is part of `wm-host` — nothing extra to
run — and authenticates with the same bearer token as the REST API and the
CLI.

> **The URL is `https://<host>/api/mcp`, not the host root.** WireMirage
> serves OAuth discovery metadata (RFC 9728 / 8414) at the root, so a client
> pointed at `https://wm.example.com` will walk the OAuth consent flow, get a
> token, and *then* fail to use the integration, because the root doesn't
> speak MCP. The symptom is "OAuth approved, then connection failed."

```sh
claude mcp add --transport http wiremirage \
  https://wm.example.com/api/mcp \
  --header "Authorization: Bearer wmt_..."
```

A logged-in user can get paste-ready configs for several clients from the
host's own **`/ui/connect`** page, including the endpoint derived from the
request so it shows the real public origin.

Behind a reverse proxy, `WM_TRUSTED_PROXY` must include the public hostname or
the transport's DNS-rebinding guard rejects the request before auth runs —
which clients report as an opaque authorization failure. See
[configuration](configuration.md#behind-a-reverse-proxy).

## Tools (33)

**Identity and discovery**

| Tool | Purpose |
|---|---|
| `who_am_i` | Caller identity, admin flag, and the host's public `base_url` |
| `summarize_workspace` | Groups, routes, and recent activity at a glance |
| `get_capabilities` | The handler API as markdown, by topic — same content as `wm capabilities` |
| `find_route` | What would match `(group, method, path)`, with near-misses |

**Groups**

| Tool | Purpose |
|---|---|
| `list_groups`, `show_group`, `create_group`, `update_group`, `delete_group` | Group CRUD (`update_group` covers TTL, sliding, and the callout opt-in) |
| `refresh_group_ttl` | Push the expiry out without touching config |
| `import_group`, `export_group` | Whole-group spec round-trip |

**Routes**

| Tool | Purpose |
|---|---|
| `list_routes`, `show_route`, `create_route`, `update_route`, `delete_route` | Route CRUD; create/update take `source` + `language` |
| `show_route_source` | The stored handler source |
| `dry_run_route` | Run the handler against a synthetic request, against a discarded state snapshot |

**State**

| Tool | Purpose |
|---|---|
| `show_route_state`, `set_route_state`, `clear_route_state` | The route's private store |
| `show_group_state`, `set_group_state`, `clear_group_state` | The group's shared store |

**Traffic**

| Tool | Purpose |
|---|---|
| `wait_for_request` | Block until N matching entries arrive, or timeout |
| `tail_journal` | Stream entries until idle or max-entries |
| `list_journal` | Page a group's stored journal after the fact — same filters as REST |
| `clear_journal` | Wipe a group's journal |
| `list_recent_unmatched` | Requests that matched nothing, with near-miss hints |
| `show_unmatched` | The full captured envelope for one unmatched request (admin) |
| `list_callbacks`, `show_callback` | Outbound webhook delivery outcomes |

Everything is owner-or-admin gated the same way the REST surface is. **User
management is deliberately absent** — admins do that from the CLI or UI
([ADR-0015](adr/0015-cli-skill-primary-mcp-secondary.md)).

## Notes on the streaming tools

`wait_for_request` and `tail_journal` subscribe to an in-process broadcast bus
and return accumulated entries when their stop condition fires (count plus
timeout; max-entries plus idle timeout). They see traffic that arrives *while
they're waiting* — for a request that already completed, use `list_journal`.
Under multiple replicas the bus fans out over Valkey pub/sub
([ADR-0037](adr/0037-multi-replica-readiness.md)), so a tool sees traffic
dispatched by any replica rather than only the one serving it.

Both can block for up to 300 seconds. A call that sends nothing for that long
is indistinguishable from a hung server, and MCP clients and reverse proxies
alike cut it short on a read timeout — so when the caller supplies a
`progressToken`, both tools emit a `notifications/progress` heartbeat: one
immediately, before blocking, then every 15 seconds while they wait. The first
one is the response's first byte, which is what a client's first-byte timer is
waiting for.

The heartbeat carries a status message only — never journal entries. Entries
still arrive in a single terminal result, so the tools behave exactly as
described above; the heartbeat only keeps the connection credibly alive.
Callers that send no `progressToken` get no notifications and a plain JSON
response, unchanged.

This does not reintroduce transport state. Progress notifications are
*request-scoped*: they travel on the response stream of the request that asked
for them, not on a server-initiated channel between requests, so the stateless
transport ADR-0037 chose is untouched and any replica can serve the call.

## Values on the wire

Bytes — request/response bodies, state values — cross the JSON boundary as a
**UTF-8 string, or `{"base64": "..."}`** for binary
([ADR-0026](adr/0026-string-first-body-encoding.md)). Never as arrays of
integers. That keeps journal entries and state dumps readable (and cheap) on
an agent surface.
