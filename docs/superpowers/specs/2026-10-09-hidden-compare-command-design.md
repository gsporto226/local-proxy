# Hidden request comparison command

## Goal

Add a hidden, generic CLI command that compares a request sent directly to its configured provider with the request prepared by local-proxy, then compares the upstream cache usage reported by both calls.

## Scope

- Support any routed provider/model whose request fixture is in the provider's native wire format.
- Keep the subcommand hidden from the top-level CLI help.
- Require explicit confirmation before sending live requests.
- Read a JSON request fixture from a caller-provided path; do not persist its prompt content or print it in reports.
- Report request-shape differences with sensitive prompt content redacted, along with HTTP status, latency, input/output tokens, cached-read/write tokens, and token-weighted cache-read percentage.
- Reuse the proxy's real request-preparation path so results do not drift from normal proxy traffic.
- Require `{{compare_tag}}` in the fixture before any live run. Replace it with one stable, arm-specific tag so repeated direct and proxy calls warm separate cache prefixes.

## Non-goals

- Capturing arbitrary direct traffic from external clients.
- Sending comparison requests automatically during normal proxy operation.
- Storing comparison prompts or results in the ordinary request statistics database.
- Comparing across different credentials or provider accounts.

## Security and behavior

- The command must not emit request bodies or authorization headers to tracing logs.
- Live API usage is explicit and the reported call count is visible before sending.
- The direct arm uses the selected provider's configured credentials while bypassing proxy body transformations; the proxy arm uses the same provider/account and the shared preparation logic.
- The request format must match the provider's wire format. Fail before network activity when it does not.
- Upstream HTTP errors are reported by status without printing response bodies, which may contain sensitive data.
- No comparison call writes to `stats.db`.

## Acceptance criteria

1. `local-proxy --help` does not list the comparison subcommand; its own help shows generic model, format, fixture, repeat, and live-confirmation options.
2. Missing confirmation, invalid fixture JSON, unknown model, unsupported/mismatched format, or missing compare-tag placeholder causes zero upstream calls.
3. Direct and proxy requests use the same routed provider, model, and account.
4. The report distinguishes request-level hit from `cache_read / input_tokens` and labels absent usage as unreported, not a miss.
5. Report and log output never contains fixture prompt content or upstream response bodies.
6. Offline tests cover argument validation, request preparation/redaction, usage calculation, and no stats persistence.

## Usage

The fixture is a JSON object in the selected provider's native wire format. It
must contain `{{compare_tag}}` in a cacheable prompt prefix so each arm receives
an isolated cache key.

Example fixture (`claude/haiku-5.5`, Anthropic Messages format):

```json
{
  "model": "claude/haiku-5.5",
  "max_tokens": 32,
  "system": [{
    "type": "text",
    "text": "stable instructions {{compare_tag}}",
    "cache_control": {"type": "ephemeral"}
  }],
  "messages": [{"role": "user", "content": "say hi"}]
}
```

Invocation (hidden subcommand, not listed in `--help`):

```sh
local-proxy compare \
  --model claude/haiku-5.5 \
  --format anthropic \
  --request fixture.json \
  --effort low \
  --runs 2 \
  --confirm-live
```

The command prints `2 * runs` live requests to stderr before sending, then
outputs a JSON report with a redacted structural diff and per-arm cache usage.
No prompts, credentials, or response bodies appear in the report or logs.
