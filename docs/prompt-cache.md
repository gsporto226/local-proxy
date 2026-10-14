# Check prompt cache reuse

Preview a generated comparison without loading credentials or making requests:

```sh
local-proxy cache-check --model chatgpt/gpt-6-sol --dry-run
local-proxy cache-check --model claude/haiku-5.5 --dry-run --json
```

To run a comparison, replace `--dry-run` with `--confirm-live`. The command
uses the selected account unless you pass `--account <alias>`.

```sh
local-proxy cache-check --model chatgpt/gpt-6-sol --runs 3 --confirm-live --json
local-proxy cache-check --model claude/haiku-5.5 --runs 3 --confirm-live
```

The default sends six requests, three directly and three after proxy request
preparation. Each group uses a separate stable prefix. Its first request warms
the cache; subsequent requests test reuse. Both groups use the same account.
The command exercises the request preparation code, not a running proxy's HTTP
endpoint, and does not write comparison usage into `stats.db`.

The generated prefix defaults to an estimated 8,192 tokens. Use
`--prefix-tokens` to choose between 1,024 and 32,768 estimated tokens. The
estimate uses four bytes per token, not the model's tokenizer. Check the
reported input count against the minimum for your model. `--runs` accepts 2 to
20 requests per group. Live runs consume account quota and may incur charges.

Read each group's `verdict`:

- `reuse`: a repeated successful request reported cached-read tokens.
- `no_reuse_observed`: repeated requests reported zero cached-read tokens.
- `inconclusive`: a request failed or the needed usage telemetry was missing.

`cache_read_percent` is the sum of cached-read tokens divided by the sum of
input tokens for requests with cache-read telemetry. It includes the warmup.
No reuse is evidence about that experiment, not proof of a proxy defect.
Provider thresholds, expiration and routing can also affect the result.

## Read cache metrics in the panel

The Usage tab and account rows show token reuse separately from the request
hit rate. A hit counts any positive cached-read count; token reuse measures
how much of the input was read from cache. The panel also shows tokens read,
written and uncached, and the coverage of request telemetry.

These token totals cover only requests with cached-read telemetry. Requests
with no telemetry are excluded, not counted as misses. Anthropic's total input
includes uncached input, cache reads and cache writes. Token reuse does not
measure monetary savings.

Recent requests show observations such as missing markers, changed static
instructions or tools, changed provider/model/account/key, cache writes and
missing telemetry. A zero read with no write is labeled "cause unknown".
These observations can help investigate a miss but do not establish its cause.

Changes are compared with the last request received for the same nonempty
`X-Claude-Code-Session-Id`. Only hashes of instructions, tools and cache keys
are retained in memory. Up to 1,024 sessions are tracked; the history clears
when a new session exceeds the limit or the process restarts. Appended user
and assistant turns are not classified as changes to static instructions.
Diagnostic codes persist with usage rows, without prompts or hashes.

The embedded ChatGPT provider forwards that session identifier as the upstream
`session_id` header. The live size sweep first observed Luna cache reads after
adding this header, even though larger prompts without it had reported zero.
Keep session identifiers stable across turns. A complete provider override must
also include `session_header: session_id` to retain this routing setting.

## Enable Anthropic breakpoints for translated requests

Providers use `prompt_cache: preserve` by default. To add Anthropic breakpoints,
set `prompt_cache: anthropic-5m` or `prompt_cache: anthropic-1h` on an
Anthropic-format provider. This applies to native requests and requests
translated from OpenAI Chat Completions or Responses.

```yaml
providers:
  - name: my-anthropic
    base_url: https://api.anthropic.com
    format: anthropic
    models: [claude-haiku-5-5]
    prompt_cache: anthropic-5m
```

If you override a catalog provider, include its other required settings.
Provider overrides replace the complete catalog entry, including any OAuth
recipe.

The policy adds at most three breakpoints: the last tool, the last system text
block, and the latest user text block. Strings become text blocks with the
same content. Existing client `cache_control` markers, including top-level
automatic caching, bypass the policy entirely. Thinking blocks are not marked.
The policy does not pad ordinary requests or guarantee that the model's minimum
cache length is met. Cache writes can cost more than ordinary input.

Provider references: [OpenAI prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching)
and [Anthropic prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching).
