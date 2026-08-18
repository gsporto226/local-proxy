# PLAN — local-proxy

Proxy local de tradução **OpenAI ⇄ Anthropic** multi-provider, em Rust (axum). Usado por qualquer
ferramenta que fale Anthropic Messages API (`/v1/messages` — Claude Code, Claude Design) ou OpenAI
Chat Completions / Responses API (`/v1/chat/completions`, `/v1/responses` — Codex e afins).

Referência de design: [`ocgo`](https://github.com/emanuelcasco/ocgo) (proxy + CLI launcher p/
Claude Code via OpenCode Go), generalizado para **múltiplos providers configuráveis** e com
**tradução de erros** e **count_tokens real** (melhorias vs ocgo).

## Stack

- axum 0.8, tokio (full), reqwest 0.12 (features: `json`, `stream`, `rustls-tls`; default-features
  off para evitar openssl), serde/serde_json, serde_yaml (0.9; se deprecado/yanked, usar fork
  `serde_yml`), clap 4 (derive), tracing + tracing-subscriber (env-filter), tower-http (trace),
  futures-util, thiserror, uuid (v4).

## Endpoints

| Rota                         | Formato cliente            | Tradução necessária |
|------------------------------|----------------------------|---------------------|
| `POST /v1/messages`          | Anthropic Messages         | A→O (idem A→A passthrough) |
| `POST /v1/messages/count_tokens` | Anthropic               | heurística chars/4 (≈0 retorna estimativa real) |
| `POST /v1/chat/completions`  | OpenAI Chat Completions    | O→A (idem O→O passthrough) |
| `POST /v1/responses`         | OpenAI Responses API       | Responses→O→A / A→O→Responses |
| `GET /v1/models`             | Anthropic **ou** OpenAI    | shape por header `anthropic-version` presente ⇒ Anthropic, senão OpenAI; catálogo = modelos roteados |
| `GET /health`                | —                          | `ok` |

Streaming: todos os `POST` suportam `stream: true` via SSE, com tradução **evento-a-evento**.

## Config (aceita JSON)

A config principal vive no diretório de config do usuário: `%APPDATA%\local-proxy\config.yaml`
(Windows) ou `~/.config/local-proxy/config.yaml` (Unix). Os arquivos de runtime (pid + log +
`auth.json`) ficam no mesmo diretório.

**Catálogo embutido + overlay:** o binário embute um catálogo de providers (`src/catalog.yaml`,
via `include_str!`) com todos os suportados (anthropic, openai, opencode-go, zen, groq, xai, google,
deepseek, openrouter). A config do usuário é um **overlay**: provider com mesmo nome sobrescreve o do
catálogo; nome novo adiciona; rotas e `defaults` da config vencem. O `DEFAULT_CONFIG` é mínimo
(`server:` apenas).

**Resolução (precedência):**
1. Flag explícita `--config <path>`.
2. Env var `LOCAL_PROXY_CONFIG`.
3. `config.yaml`/`config.json` no diretório de trabalho (override project-local, só se existir).
4. Global default.

**Auto-criação do default:** se o arquivo global não existir, o CLI o cria a partir de um default
embutido e imprime uma mensagem dizendo onde foi criado e que basta editar e rodar de novo.

```yaml
server:
  host: 127.0.0.1
  port: 8787
  api_keys: [sk-proxy]        # auth opcional (X-API-Key / Bearer); launcher usa a primeira
  passthrough_keys: false      # true = repassa a key do cliente ao upstream

# Overlay: sobrescreve "openai" do catálogo / adiciona "zen"
providers:
  - name: zen
    base_url: https://opencode.ai/zen
    api_key_env: OPENCODE_ZEN_KEY
    format: openai
    models: [deepseek-v4-flash-free]

routes:
  - model: deepseek-free       # prefix: true casa deepseek-free-4-5 etc.
    provider: zen
    upstream_model: deepseek-v4-flash-free

defaults:
  provider: anthropic          # fallback final (nome do modelo passa inalterado)
  model: null                  # modelo ativo (persistido via MCP models(select)); "" = não usar
```

## Auth (`auth.json`)

Chaves ficam em `auth.json` no config dir global, formato do opencode:
`{ "<provider>": { "type": "api", "key": "..." } }`, com escrita atômica (temp+rename).
`connect <provider> [key]` valida que o provider existe (catálogo ∪ config) e só grava a chave
(prompt oculto via `rpassword`); `disconnect` remove.
Resolução da chave por request: `api_key` inline → `auth.json[provider]` → `api_key_env`.

## Hot-reload (file watcher)

`AppState` guarda `RuntimeState { config (merged), router, clients }` em `Arc<RwLock<...>>`; handlers
leem um snapshot por request (`state.snapshot().await`). `serve` spawna um watcher
(`notify` + `notify-debouncer-full`, debounce 300ms) no config dir + pai do `config_path`; em mudança
de `config.yaml`/`auth.json`, reconstrói o estado via `build_runtime_state` (catalog merge + router +
clients) e atualiza a lock — **sem reiniciar**. `connect`/`select` do MCP dependem disso para aplicar
em runtime.

## MCP (`local-proxy mcp`, rmcp)

Servidor MCP **stdio** (`rmcp` 3.x, `#[tool_router(server_handler)]`) com tools:
- `connect(provider, key)` / `disconnect(provider)` — mesma lógica do CLI.
- `providers()` — providers efetivos + key status.
- `models([select])` — lista modelos efetivos; com `select` valida e persiste `defaults.model` no
  config. Com `defaults.model` setado, `resolve_model` **ignora o modelo do harness** e roteia pelo
  selecionado; `select: ""` limpa.

## Roteamento (`src/router.rs`) — precedência

1. Rota exata (`routes[].model == model`)
2. Sintaxe `provider/model` no nome pedido
3. Prefixo (`routes[].prefix` casa por prefixo de família)
4. Provider cuja lista nativa `models[]` contém o modelo
5. `defaults.provider` (modelo inalterado)
6. Erro `model_not_found` (reformatado para o shape do cliente)

## Traduções (`src/translate.rs`)

### Request
- **A→O**: `system` (string ou array) → message role `system`; blocos `text` → partes/string;
  bloco `image` (`source.base64/url`) → `image_url` (data URI); bloco `tool_use` → `tool_calls`
  (arguments = JSON string); bloco `tool_result` → role `tool` (`tool_call_id`); `input_schema` →
  `parameters` (default `{"type":"object","properties":{}}` se vazio) com `type: "function"`;
  `tool_choice` auto/none/`{type:"tool"}` → `"auto"`/`"none"`/`{"type":"function",...}`;
  `stop_sequences` → `stop`; drop `top_k`, `cache_control`, `thinking`; extrair
  `reasoning_effort` de `thinking|reasoning|reasoning_effort|effort|level|depth|output_config`.
- **O→A**: inverso — role `system`/`developer` → `system`; `image_url` → bloco image
  (data URI → base64); `tool_calls` → blocos `tool_use`; role `tool` → `tool_result`;
  `stop` → `stop_sequences`; `max_tokens`/`max_completion_tokens` → `max_tokens`.
- **Responses→Chat**: `input[]` (message/function_call/function_call_output) → messages;
  `instructions` → system; `tools` (function + builtin web_search/web_fetch) → tools.

### Response
- Shapes: Anthropic `{id,type,role,model,content[],stop_reason,stop_sequence,usage}`
  ↔ OpenAI `{id,object,created,model,choices[],usage}` ↔ Responses `{id,object,created_at,model,
  status,output[],usage}`.
- `finish_reason` ↔ `stop_reason`: `stop`↔`end_turn`, `tool_calls`↔`tool_use`, `length`↔`max_tokens`.
- `usage`: um `tokenUsage{input,output,reasoning}` único + conversores por formato; OpenAI
  contabiliza `prompt_tokens/completion_tokens/total_tokens`; Anthropic `input_tokens/output_tokens`.

### Erros (`src/error.rs`)
Upstream 4xx/5xx → reformatar o body do erro para o shape do **cliente** que chamou:
- `/v1/messages`: `{"type":"error","error":{"type":"...","message":"..."}}`
- `/v1/chat/completions` e `/v1/responses`: `{"error":{"message":"...","type":"...","code":"..."}}`
Parse tolerante dos dois shapes de erro upstream (OpenAI e Anthropic).

## Streaming (`src/sse.rs` + `src/streams.rs`)

- `src/sse.rs`: parser de frames SSE sobre `reqwest::Response::bytes_stream` (linhas `event:`/`data:`,
  data multi-linha, sentinela `[DONE]`).
- **O→A** (`streamAnthropic`): sintetizar `message_start` (id fake, usage 0), `content_block_start`
  (text) no primeiro delta, `content_block_delta` `text_delta` por chunk, blocos `tool_use` a partir
  de `delta.tool_calls` incrementais (`partial_json` acumulado), `content_block_stop`,
  `message_delta` com `stop_reason` (tool_use se há tools, senão end_turn) e usage de
  `stream_options.include_usage`, `message_stop`. Acumular `reasoning_content` e re-injetar nos
  tool_calls (cache por call id).
- **A→O** (`streamChatCompletions`): chunk inicial com `delta.role=assistant`; `content_block_delta
  text_delta` → `delta.content`; `content_block_start tool_use` → `delta.tool_calls` (index/id/name);
  `input_json_delta` → `function.arguments` parcial; `message_delta.stop_reason` → `finish_reason`;
  `data: [DONE]`.
- **Responses** (2 direções): eventos `response.created`, `response.output_item.added`,
  `response.content_part.added`, `response.output_text.delta/done`, `response.function_call_arguments.delta/done`,
  `response.output_item.done`, `response.completed`.

Truncar `tool_result` a 120k chars ao reenviar (Anthropic impõe limite).

## Upstream (`src/upstream.rs`)

- Reqwest client por provider (timeout ~10min p/ inferência).
- Anthropic: headers `x-api-key`, `anthropic-version: 2023-06-01`, `content-type`.
- OpenAI: `Authorization: Bearer`.
- `passthrough_keys=true`: repassar a key do cliente ao upstream em vez da key configurada.
- Antes de forward A→A (provider anthropic), normalizar request: strip `thinking/reasoning/effort/
  level/depth/output_config` e `cache_control` (upstreams rígidos rejeitam).

## CLI (`src/main.rs`, clap)

- `serve [--config <path>] [--background]` — roda o proxy; `--background` spawna processo
  desacoplado (pid/log no diretório de config global).
- Erros de CLI reportados com **miette** (rico diagnóstico): códigos `config::io`, `config::parse`,
  `cli::...`, contexto com o trecho-fonte em erros de parse YAML/JSON. O shape `ApiError`
  (Anthropic/OpenAI) voltado ao cliente HTTP permanece inalterado.
- `launch claude [--model X] [--yes] [-- args...]` — sobe serve, seta `ANTHROPIC_BASE_URL`,
  `ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_API_KEY` (proxy key ou "unused"), `ANTHROPIC_MODEL`/
  `ANTHROPIC_SMALL_FAST_MODEL` se `--model`, executa `claude args`.
- `launch design [-- args...]` — idem p/ Claude Design (`ANTHROPIC_BASE_URL` + auth).
- `status` / `stop` — pid file.
- `models` — lista modelos roteados (efetivos).
- `connect <provider> [key]` / `disconnect <provider>` / `providers` — auth store (`auth.json`).
- `mcp` — servidor MCP stdio (rmcp).
- `update [--check] [--force] [--repo owner/repo] [--no-verify]` — baixa o binário mais recente do
  último release do GitHub (padrão `gsporto226/local-proxy` ou `$env:LOCAL_PROXY_REPO`), verifica
  SHA256 (`sha2`) e aplica no lugar: no Linux o swap é um rename POSIX atômico (funciona com o
  binário em execução); no Windows o executável em uso é renomeado para um backup (`.old`) e o novo
  assume a posição, com o backup apagado por helper destacado e no próximo start. Instalações via
  cargo (`~/.cargo/bin` ou `$CARGO_HOME/bin`) delegam ao `cargo install --force`. `--check` só
  informa a versão. `serve --check-update` avisa no log se houver release mais nova
  (desativável com `$env:LOCAL_PROXY_DISABLE_AUTOUPDATE=1`).
  Erros com códigos `update::unsupported/fetch/no_asset/verify/download/stage/replace`.

## Módulos

```
src/
├── main.rs        CLI + boot (erros com miette, códigos config::/cli::)
├── config.rs      Config/Provider/Route/Defaults (YAML/JSON) — overlay, auto-created
├── catalog.rs     catálogo embutido (catalog.yaml) + merge catálogo↔config
├── auth.rs        auth.json (keys) + escrita atômica
├── cli.rs         serve/launch/status/stop/models/connect/disconnect/providers/mcp/update
├── router.rs      resolve_model → (Provider, upstream_model)
├── upstream.rs    chamada HTTP + resolução de chave (inline > auth > env)
├── translate.rs   requests/responses A↔O↔Responses
├── sse.rs         parser de frames SSE
├── streams.rs     máquinas de estado de streaming (3 direções)
├── mcp.rs         servidor MCP stdio (rmcp): connect/disconnect/providers/models(select)
├── error.rs       ApiError + shape por formato
└── handlers.rs    axum handlers + RuntimeState (RwLock) + watcher hot-reload + /v1/models + count_tokens
```

## Fases (kanban)

1. **001 Scaffold**: cargo, config load, router, CLI skeleton, PLAN.md
2. **002 Núcleo não-streaming**: translate + handlers + erro formatado + /v1/models + count_tokens
3. **003 Streaming**: SSE + máquinas de estado nas 3 direções
4. **004 CLI launcher**: serve bg + launch claude/design + status/stop
5. **005 Testes + docs (QA)**: unit, integração e2e (mock upstream axum), README, config.example.yaml
6. **006 Catálogo embutido + overlay**: catalog.yaml/catalog.rs + merge + DEFAULT_CONFIG mínimo
7. **007 Auth + connect**: auth.json, connect/disconnect/providers, resolução inline>auth>env
8. **008 Hot-reload**: RuntimeState em RwLock + file watcher (notify) sem reinício
9. **009 MCP + select**: servidor rmcp stdio + models(select) persistido + override de roteamento
10. **010 QA**: unit novos + e2e + docs + gate AGENTS.md

## Fora de escopo (v1)

Retry/backoff, round-robin entre múltiplas chaves, rate limit, embeddings, cache, imagem Podman.