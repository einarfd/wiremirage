# The `wm` CLI

`wm` wraps the control-plane REST API. It's a thin Rust binary with no native
dependencies, so it compiles quickly compared to the host.

## Install

Pre-built binaries are attached to each [release][releases] for macOS and
Linux (x86_64 and aarch64). The Linux builds are statically linked against
musl, so they carry no glibc version floor.

```sh
# aarch64-apple-darwin | x86_64-apple-darwin
# aarch64-unknown-linux-musl | x86_64-unknown-linux-musl
TARGET=aarch64-apple-darwin

curl -fsSL "https://github.com/einarfd/wiremirage/releases/latest/download/wm-$TARGET.tar.gz" \
  | tar xz
sudo install "wm-$TARGET/wm" /usr/local/bin/wm
```

Each archive also carries `LICENSE`, `NOTICE`, and the README;
`checksums.txt` on the release page covers every archive.

From source instead:

```sh
cargo install --path crates/wm-cli          # from a clone
cargo install --git https://github.com/einarfd/wiremirage wm-cli
```

That puts `wm` in `~/.cargo/bin/`. `wm --version` confirms.

[releases]: https://github.com/einarfd/wiremirage/releases

Shell completions:

```sh
wm completion bash > /etc/bash_completion.d/wm
wm completion zsh  > "${fpath[1]}/_wm"
wm completion fish | source
```

## Connecting

```sh
export WM_HOST=http://localhost:8080   # default; the control-plane origin
export WM_TOKEN=wmt_...                # bearer token
wm health                              # no token needed
```

Both can be passed inline as `--host` / `--token`, or stored in
`~/.config/wiremirage/config.toml` as named profiles — see
[configuration](configuration.md#cli-configuration).

`--json` on any command switches to machine-parseable output (the contract for
scripts and agents); the default is column-aligned text. Exit codes: `0` ok,
`1` generic error, `2` usage, `4` auth, `5` not found, `6` conflict.

## Command tour

```sh
# Groups — the lifecycle unit. Deleting one cascades routes, state, journal.
wm groups create stripe-mock --ttl-seconds 3600
wm groups list --sort last_activity_at --dir desc
wm groups show stripe-mock
wm groups update stripe-mock --ttl-seconds 7200 --callout
wm groups refresh stripe-mock
wm groups export stripe-mock --format yaml > stripe-mock.yaml
wm groups create --from-file stripe-mock.yaml     # import, atomic
wm groups delete stripe-mock --force

# Routes
wm routes add --group stripe-mock --method POST --path /v1/charges \
  --source-file handler.ts
wm routes list --group stripe-mock --sort hits_total --dir desc
wm routes show stripe-mock/1
wm routes source stripe-mock/1                    # print stored handler source
wm routes update stripe-mock/1 --source-file v2.ts
wm routes test stripe-mock/1 --method POST --body '{"x":1}' --kv counter=4
wm routes delete stripe-mock/1

# State — seed a mock's config without driving traffic through it
wm routes state stripe-mock/1
wm routes state stripe-mock/1 --set mode=degraded
wm routes state stripe-mock/1 --snapshot > baseline.json
wm routes state stripe-mock/1 --reset-from baseline.json
wm routes state stripe-mock/1 --clear
wm groups state stripe-mock ...                   # same flags, shared store

# What actually happened
wm journal list stripe-mock --since 5m --status 5xx
wm journal show stripe-mock/12
wm unmatched list --path-pattern '/v1/*'          # requests that matched nothing
wm unmatched show 3
wm callbacks list stripe-mock                     # outbound webhook deliveries
wm match -g stripe-mock POST /v1/charges          # what would match, and near-misses

# Admin
wm tokens create ci-runner                        # plaintext printed once
wm tokens list / rename / revoke
wm users list / show / me / create / update / delete
wm capabilities store                             # live handler API docs
```

The list commands share a filter vocabulary — `--method`, `--path-pattern
'/v1/*'`, `--status 5xx`, `--since 5m`, `--until` — and differ in how they
page. `wm routes list` / `wm groups list` use offset pagination (`--sort
<column> --dir asc|desc --limit --offset`) and print a `(showing K of N;
--offset M for the next page)` footer; `wm journal list` / `wm unmatched
list` are cursor-based (`--before <n> --limit`), newest first.
`wm <command> --help` is the source of truth.

## What the CLI deliberately doesn't do

- **Live journal tailing.** Watching a stream is a push concern, served by the
  MCP `tail_journal` / `wait_for_request` tools and the web UI's live journal
  page. The CLI stays request/response (`wm journal list` / `show`).
- **User management is CLI-and-UI only** — the MCP surface deliberately omits
  it ([ADR-0015](adr/0015-cli-skill-primary-mcp-secondary.md)); agents
  shouldn't be creating users.
