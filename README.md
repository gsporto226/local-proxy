# local-proxy

A local translation proxy for AI APIs. It speaks the Anthropic Messages API and the OpenAI Chat Completions and Responses APIs, then routes each request to any provider that uses either wire format.

Claude Code talks `/v1/messages`. Codex and opencode talk `/v1/chat/completions` and `/v1/responses`. Each tool is locked to its vendor's format, so you can't just point it at another provider. The proxy sits in the middle: it accepts one format, translates the request, calls the upstream provider, and translates the response back. Streaming stays streaming, event by event.

You keep the tool you like and pick the provider you want, per model, per request, without touching the tool's config.

## Why

Most AI tools hardcode their vendor's API shape. The reference design here is [ocgo](https://github.com/emanuelcasco/ocgo), generalized to many configurable providers, with real error translation and a working `count_tokens`. Instead of a second tool per vendor, one proxy speaks all the wire formats and forwards to any of them.

## Features

- Endpoints: `/v1/messages`, `/v1/messages/count_tokens`, `/v1/chat/completions`, `/v1/responses`, `/v1/models`, `/health`.
- Request and response translation plus streaming SSE, event by event, in all three directions (Anthropic to OpenAI, OpenAI to Anthropic, Responses to Anthropic and OpenAI).
- Embedded provider catalog (`anthropic`, `openai`, `opencode-go`, `zen`, `groq`, `xai`, `google`, `deepseek`, `openrouter`, `neuralwatt`). Your `config.yaml` only adds to it or overrides entries; it never replaces the whole list.
- Hot reload. Editing the config or `auth.json` applies in runtime through a file watcher, no restart.
- Auth store separate from the config (`auth.json`), modeled after opencode's `/connect`. Keys never live in the config.
- `$proxy` executor. When the last user message starts with `$proxy `, the proxy runs the rest as a `local-proxy` command and returns the output as the model's reply. Works with no provider connected.
- Model routing with a clear precedence (exact route, `provider/model`, prefix, native list, default).
- Upstream errors reformatted into the client's shape (Anthropic or OpenAI).
- Optional auth (`X-API-Key` or `Authorization: Bearer`) and `passthrough_keys`.
- Usage statistics in a local SQLite database (`stats.db`): endpoint, provider, model, tokens in and out, latency, streaming, status, per request, queryable by time window.

## Requirements

- Rust stable to build from source.
- Bun only for the e2e suite.
- Release binaries are published for x86_64 only (linux, darwin, windows).

## Install

Three ways to get `local-proxy`.

### Prebuilt binary

The installers fetch the latest release from GitHub, verify the SHA256, and install to `~/.local/bin`.

Linux and macOS:

```bash
curl -fsSL https://raw.githubusercontent.com/gsporto226/local-proxy/main/install.sh | bash
```

Windows, PowerShell (any version, 5.1 included):

```powershell
irm https://raw.githubusercontent.com/gsporto226/local-proxy/main/install.ps1 | iex
```

`install.sh` respects `LOCAL_PROXY_REPO` for a different owner/repo, `LOCAL_PROXY_INSTALL_DIR` for a target directory, and `LOCAL_PROXY_SKIP_VERIFY=1` to skip the SHA256 check. `install.ps1` takes the same options as parameters: `-Repo`, `-Tag`, `-InstallDir`, `-AddToPath`, and `-SkipVerify`.

### Cargo

```bash
cargo install local-proxy
```

### From source

```bash
cargo build --release
```

The binary lands in `target/release/local-proxy` (`local-proxy.exe` on Windows).

## Quick start

```bash
local-proxy connect opencode-go          # prompts for the API key
local-proxy serve                        # proxy on 127.0.0.1:8787
```

Point Claude Code at it:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8787
export ANTHROPIC_AUTH_TOKEN=sk-proxy     # the server api_key, or "unused" with no auth
claude
```

Claude Code sends `/v1/messages`; the proxy routes to a connected provider and translates. `local-proxy models` lists what you can use, and `local-proxy model <provider>/<model>` selects the active one.

## CLI reference

| Command | Purpose |
| --- | --- |
| `serve` | Run the proxy server, foreground by default. `--host`, `--port`, `--background`, `--check-update`. |
| `launch <claude\|design>` | Start a dedicated instance on a random free port, run the tool against it, kill the proxy when the tool exits. `--model`, `--yes`, `--dry-run`, `-- args...`. |
| `status` | Show whether the background proxy is running. |
| `stop` | Stop the background proxy. |
| `models` | List models from connected providers, as `provider/model`. |
| `model [<m>]` | Show or set the active model. `model clear` unsets it. |
| `connect <provider> [key]` | Store an API key for an existing provider. Prompts hidden if the key is omitted. |
| `disconnect <provider>` | Remove the stored API key. |
| `providers` | List effective providers (catalog plus config) with key status. |
| `stats [--since day\|week\|month\|all] [--json]` | Show usage statistics from recorded requests. |
| `statusline --session <uuid>` | Render the Claude Code status line for a session from its recorded stats. |
| `update` | Check for and apply a newer release. |

## Configuration

The provider catalog is compiled into the binary from `src/catalog.yaml`. Your config is an overlay. A provider with the same name replaces the catalog entry. A new name adds a provider. Routes and defaults defined in your config win over the catalog's. Keys come from the auth store or inline `api_key`; there is no environment variable fallback.

The main config lives in the user config directory: `%APPDATA%\local-proxy\config.yaml` on Windows, `~/.config/local-proxy/config.yaml` on Unix. The runtime files (pid, log, `auth.json`, `stats.db`) live in the same directory. If the global file does not exist, the CLI creates it from a minimal embedded default and prints where it was created.

Config resolution, in order:

1. Explicit `--config <path>` flag.
2. `LOCAL_PROXY_CONFIG` environment variable.
3. `config.yaml` or `config.json` in the working directory, only if it exists.
4. The global default file.

The global config directory can be redirected with `LOCAL_PROXY_CONFIG_DIR`, which isolates test data without touching the real directory.

Example: add a custom provider and a route.

```yaml
providers:
  - name: zen
    base_url: https://opencode.ai/zen
    format: openai
    models: [deepseek-v4-flash-free, claude-sonnet-4-5]
routes:
  - model: deepseek-free
    provider: zen
    upstream_model: deepseek-v4-flash-free
defaults:
  provider: anthropic          # final fallback, model name passes unchanged
```

A copy of this example ships as `config.example.yaml` in the repo.

### Per-provider headers

Any provider can send static headers on every request through `headers:`. A header with the same name overrides the default auth or format header. OpenRouter needs the routing headers `HTTP-Referer` and `X-Title`; the embedded catalog already sends them:

```yaml
providers:
  - name: openrouter
    base_url: https://openrouter.ai/api/v1
    format: openai
    headers:
      HTTP-Referer: https://github.com/gsporto226/local-proxy
      X-Title: local-proxy
    models: [openrouter/auto]
```

### Keys: connect and disconnect

Keys live in `auth.json`, never in the config. `connect` only accepts providers that already exist, in the catalog or added by your config.

```bash
local-proxy connect opencode-go          # prompts hidden
local-proxy connect opencode-go sk-xxx   # or pass it directly
local-proxy providers                    # effective providers plus key status
local-proxy disconnect opencode-go       # remove the key
```

Per request, the key resolution reads `auth.json[provider]`. That is the only source.

### Active model

The proxy never uses the model the harness asks for, for example `ANTHROPIC_MODEL` from Claude Code. It routes by the active model, written as `provider/model`: the explicitly selected model, else the first model from a connected provider, else an error. A provider counts as connected when it has a key in `auth.json`.

Selection and query happen through the CLI or through `$proxy`, with the same validation logic:

```bash
local-proxy model                                # selected model, else first available, else "none"
local-proxy model opencode-go/deepseek-v4-flash  # validates against connected providers, sets active
local-proxy model clear                          # back to first available
local-proxy models                               # models from connected providers
```

Inside a request, `$proxy model` reports this instance's active model in memory, and `$proxy model <m>` selects and persists it.

The selection is stored in `defaults.active_model` in the config. Last write wins on the next start. Each proxy instance keeps its own active model in memory and does not propagate it to other instances through hot reload. Running `local-proxy model X` from the CLI only writes the config and does not change a proxy that is already running.

### Hot reload

Any edit to `config.yaml` or `auth.json`, by CLI or by hand, applies in runtime through a file watcher with a 300ms debounce. No restart. One exception: `defaults.active_model` is not reread from the file. Each instance keeps the active model it set in memory.

## HTTP endpoints

| Endpoint | Purpose |
| --- | --- |
| `POST /v1/messages` | Anthropic Messages API. |
| `POST /v1/messages/count_tokens` | Anthropic token counting, routed like a normal message. |
| `POST /v1/chat/completions` | OpenAI Chat Completions. |
| `POST /v1/responses` | OpenAI Responses API. |
| `GET /v1/models` | List available models. |
| `GET /health` | Liveness check. |

## Using with Claude Code

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8787
export ANTHROPIC_AUTH_TOKEN=sk-proxy
claude
```

Claude Code talks `/v1/messages`; the proxy routes to the configured provider and translates, for example `deepseek-free` to `deepseek-v4-flash-free` through opencode-zen.

### Launch: one dedicated instance

`local-proxy launch claude [--model X] [--yes] [-- args...]` (and `launch design`) starts a dedicated proxy instance on a random free port, points the tool at it, and kills the proxy when the tool exits, including on error or non-zero exit. It never reuses or touches a background proxy that is already running, and never overwrites the shared pid file. Statistics still go to the same `stats.db` (WAL plus busy timeout, safe with multiple instances):

```bash
local-proxy launch claude --model kimi-k2.6 --yes
```

## The `$proxy` executor

On any endpoint, if the last user message starts with the token `$proxy `, the proxy does not forward to a provider. It runs the rest as a `local-proxy` command and returns the output as the model's reply, in all three wire formats. Works with no provider connected.

```bash
$proxy status                        # proxy status
$proxy models                        # models from connected providers
$proxy stats --since week            # usage statistics
$proxy model deepseek-v4-flash       # select this instance's active model, persisted
$proxy connect opencode-go <key>     # store the key, no interactive prompt
```

The token, the binary, and the timeout are configurable in the `exec` block:

```yaml
exec:
  enabled: true        # on by default
  token: "$proxy"      # prefix that triggers execution
  command: local-proxy # binary executed
  timeout_secs: 30     # kill after the timeout
```

Security: it only runs `exec.command` (default `local-proxy`) with args parsed without a shell. No arbitrary execution. It requires the proxy key like any endpoint.

## Statistics and status line

The proxy records every proxied request in the local `stats.db` in the global config directory. Best effort: a write failure is logged and never breaks the proxy. Tokens come from the upstream `usage`: in the response body for non-streaming requests, and for streaming by accumulating `usage` from the SSE frames (OpenAI chunks, Anthropic `message_start` and `message_delta`, Responses `response.completed`). Requests are recorded when the stream finishes.

Providers with energy-based pricing (NeuralWatt) return energy and cost metadata: top-level `energy` and `cost` fields in non-streaming responses, and SSE comments (`: energy {...}` / `: cost {...}`) in streaming. The report shows totals for energy (kWh) and cost (USD) per window and per provider when present. For OpenAI-compatible providers that report a cost in `usage.cost` (OpenRouter, Groq), the proxy persists it. It never estimates cost from a price table.

```bash
local-proxy stats               # today's summary, per provider, and recent requests
local-proxy stats --since week  # day | week | month | all
local-proxy stats --json        # same report as JSON, keys summary/providers/recent
```

### Status line

The Claude Code status line is computed client-side from Claude's own price table, which is wrong when traffic goes through a multi-provider proxy. The proxy renders the line from the stats it records instead.

The flow: Claude Code sends `X-Claude-Code-Session-Id` (a UUID per session) on every request. The proxy stores `session_id` on each `stats.db` row. The status line script passes its `session_id` to `local-proxy statusline`, which aggregates that session's stats and renders a Rhai template, sandboxed.

```bash
local-proxy statusline --session "<uuid>" --model "claude-..." --context-pct 42
local-proxy statusline --session "<uuid>" --template "{model} · {cost_session} · {context_pct}% ctx"
```

Template params (absent values render as `?`): `cost_session`, `cost_month`, `cost_total`, `cost_known`, `tokens_in`, `tokens_out`, `requests`, `model`, `context_pct`. Cost only appears when the upstream reports it; it is never estimated. Formatting is entirely the template's job.

The template can come from the config (`statusline:` block) or from `--template`, which wins:

```yaml
statusline:
  template: "{model} · {cost_session} · {context_pct}% ctx"
```

Ready scripts that read the JSON from stdin, extract `session_id`, and call the binary live in `scripts/statusline.sh` and `scripts/statusline.ps1`. Point Claude Code's `settings.json` at one of them:

```json
{ "statusLine": { "type": "command", "command": "/abs/path/scripts/statusline.ps1" } }
```

## Update

`update` downloads the latest release binary from GitHub, verifies the SHA256, and applies it in place. On Linux the swap is atomic (POSIX rename, safe while the binary is running). On Windows the running executable is renamed to a backup and the new one takes its place, with cleanup of the backup on exit and at the next start. Cargo installs delegate to cargo.

```bash
local-proxy update --check        # report the latest version only
local-proxy update                # download, verify, apply
local-proxy update --force        # update even on the latest version
local-proxy update --repo owner/repo  # alternate repository, or $LOCAL_PROXY_REPO
local-proxy update --no-verify    # skip SHA256 verification
```

`serve` accepts `--check-update` to warn in the log when a newer release exists. Disable it with `$env:LOCAL_PROXY_DISABLE_AUTOUPDATE=1`.

## Development

```bash
cargo build
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

The e2e suite runs with Bun against a deterministic mock upstream:

```bash
cd e2e
cargo build --manifest-path ../Cargo.toml
bun install
bun test mock.test.ts
```

A live suite runs against opencode-zen with a real key:

```bash
cd e2e
$env:OPENCODE_ZEN_KEY="sk-..."; bun test live-zen.test.ts
```

The free upstream may rate-limit (`FreeUsageLimitError`); in that case the live tests report an environmental skip, which is not a proxy failure. The suite also includes tests that launch the real `claude` CLI against the proxy over `ANTHROPIC_BASE_URL`.

The CI workflow in `.github/workflows/ci.yml` runs fmt, clippy with `-D warnings`, and the unit tests on Ubuntu and Windows, plus the mock e2e suite, and a scheduled `cargo-audit` job. Run the same commands locally before pushing.

## Project structure

```
src/
├── main.rs        CLI and boot (errors with miette)
├── config.rs      Config, Provider, Route, Defaults (YAML/JSON), overlay, per-provider headers
├── catalog.rs     embedded catalog and catalog to config merge
├── auth.rs        auth.json keys and atomic writes
├── cli.rs         serve, launch, status, stop, models, model, connect, disconnect, providers, stats, statusline, update
├── router.rs      resolve_model to (provider, upstream_model)
├── upstream.rs    HTTP calls, key resolution, per-provider headers
├── translate.rs   request and response translation across the three formats
├── sse.rs         SSE frame parser
├── streams.rs     streaming state machines, all three directions
├── exec.rs        $proxy executor, token detection, arg parsing, timeout
├── error.rs       ApiError and per-format error shape
├── stats.rs       local statistics (SQLite stats.db) and stats command
├── statusline.rs  sandboxed Rhai template for the status line
└── handlers.rs    axum endpoints, hot-reload state, /v1/models, count_tokens, $proxy
e2e/               Bun test suite (mock and live)
scripts/           status line scripts (bash and PowerShell)
```

## Troubleshooting

- `FreeUsageLimitError` from live tests. The free opencode-zen model is rate-limited. The tests report an environmental skip; the proxy is not at fault.
- `Invalid API key` or 401s. The key lives in `auth.json`, not in the config. Run `local-proxy providers` to see which providers have a resolvable key, then `local-proxy connect <provider>`.
- Port already in use. The default port is 8787. Pass `--port` to `serve`, or use `launch`, which picks a free random port.
- Where is the config? `%APPDATA%\local-proxy\config.yaml` on Windows, `~/.config/local-proxy/config.yaml` on Unix. Override with `--config`, `LOCAL_PROXY_CONFIG`, or `LOCAL_PROXY_CONFIG_DIR`.
- `status` says "not reachable" right after `--background`. The background process may still be starting. Check again in a second.
- A `local-proxy.old` file on Windows. That is the previous executable, renamed while the new one took its place. It is cleaned up on exit and at the next start.

## Limitations

The known gaps, tracked in `docs/PENDING.md`, are not in scope for the current version:

- `model` rewrites the config through serde, so manual comments and formatting in `config.yaml` are lost on that write.
- Concurrent `connect` and `disconnect` calls can race on `auth.json`. Atomic writes prevent corruption, but there is no lock.
- The launcher keeps a single pid file, so multiple background proxies overwrite each other.
- No retry or round-robin across keys, no rate limiting, no embeddings, no cache, no container image.

## Contributing

Open an issue or a pull request. Before pushing, run the exact commands CI runs: `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all-features`, plus the mock e2e suite. The project enforces `missing_docs = "deny"` for public items. Full details in `AGENTS.md`.

## License

MIT.