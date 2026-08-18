# PENDING / status de trabalho

Estado atual do proxy `local-proxy`. Última atualização: 2026-08-18.

## ✅ TODAS AS TASKS CONCLUÍDAS (kanban: backend 001-004 + 006-009, qa 005 + 010 — todas `done`)

### Commits
- `3483446` — (001) scaffold: cargo, config YAML/JSON, router, CLI skeleton
- `661821e` — (002) núcleo não-streaming: tradução, handlers, erro formatado, /v1/models, count_tokens
- `86b49a7` — (003) streaming SSE + máquinas de estado nas 3 direções
- `b762f24` — (005) suíte e2e Bun (mock + live opencode-zen), README, config zen, docs
- `2fb1a51` — (004) CLI launcher: serve bg, launch claude/design, status/stop/models
- *pendente* — (006) catálogo embutido + overlay de config
- *pendente* — (007) auth store (auth.json) + connect/disconnect + resolução de chave
- *pendente* — (008) hot-reload com file watcher (notify) + RuntimeState em RwLock
- *pendente* — (009) servidor MCP (rmcp) connect/disconnect/models(select)/providers
- *pendente* — (010) testes unit novos + e2e + docs + validação AGENTS.md

### Nova rodada — catálogo embutido + overlay (006)
- `src/catalog.yaml` embutido no binário com todos os providers suportados pelos 2 wire formats:
  anthropic, openai, **opencode-go** (`https://opencode.ai/zen/go`, `OPENCODE_ZEN_KEY`), zen, groq,
  xai, google, deepseek, openrouter. Sem GitHub Copilot (OAuth proprietário, fora dos formats).
- `catalog::load()` + `catalog::effective_config(base, overlay)`: a config do usuário vira **overlay**
  (adiciona provider novo / sobrescreve mesmo nome / rotas e defaults da config vencem).
- `Provider.api_key` (inline) e `Defaults.model` (modelo ativo) novos campos.
- `DEFAULT_CONFIG`/`config.example.yaml` mínimos (`server:`); o padrão vem do catálogo.

### Nova rodada — auth store + connect (007)
- `src/auth.rs`: `auth.json` no config dir global, formato opencode `{provider:{type:api,key}}`,
  escrita atômica (temp+rename).
- `local-proxy connect <provider> [key]` (valida que o provider existe no catálogo ou config; prompt
  oculto via `rpassword`), `disconnect`, `providers`.
- Resolução de chave por request: `api_key` inline → `auth.json[provider]` → `api_key_env`.

### Nova rodada — hot-reload (008)
- `RuntimeState { config, router, clients }` em `Arc<RwLock<...>>`; handlers leem snapshot por request.
- File watcher (`notify` + `notify-debouncer-full`, debounce 300ms) no config dir; mudanças em
  `config.yaml`/`auth.json` reconstroem o estado **sem reiniciar** (verificado em runtime: edição de
  config aplicou em /v1/models sem restart).

### Nova rodada — MCP + select (009)
- `local-proxy mcp`: servidor MCP stdio com `rmcp` 3.x; tools `connect`, `disconnect`, `providers`,
  `models([select])`.
- `models(select)` valida e persiste `defaults.model` no config; com o campo setado o proxy **ignora
  o modelo do harness** e roteia pelo selecionado (persiste entre reinícios; aplica em runtime via
  watcher).

### Estado de verificação (task 010)
- `cargo test --all-features`: **91** unit tests verdes.
- `cargo clippy --all-targets --all-features -- -D warnings`: limpo.
- `cargo fmt --all -- --check`: limpo.
- `bun test e2e/mock.test.ts`: **17** testes determinísticos verdes.
- `bun test e2e/live-zen.test.ts`: 6 testes contra opencode-zen real — **environmental skip** enquanto o
  modelo free estiver em `FreeUsageLimitError` (rate limit); validam de verdade quando a quota permitir.

### Entregas acumuladas
- Proxy completo (endpoints, tradução A↔O↔Responses, streaming, erros, auth, count_tokens, /v1/models).
- CLI launcher (`serve`, `launch claude|design`, `status`, `stop`, `models`, `connect`, `disconnect`,
  `providers`, `mcp`, `update`).
- Catálogo embutido + overlay, auth.json, hot-reload, servidor MCP com select persistido.
- Suíte e2e em Bun, README, config.example.yaml, docs/PLAN.md, docs/PENDING.md.

## Pendências futuras (não-bloqueantes / fora do escopo v1)
- `models(select)` (persistir `defaults.model`) reescreve o config via serde — **comentários/formação
  manuais do config.yaml são perdidos** nessa escrita (aceito; documentado).
- Tools MCP podem ser despachadas concorrentemente pelo cliente; `connect`+`disconnect` simultâneos
  podem raciar no `auth.json` (escrita atômica previne corrupção; possível lock no futuro).
- Confirmar `stream_options.include_usage` contra opencode-zen com um 200 limpo (o free rate-limitou antes).
  Se o zen rejeitar, tornar `include_usage` opt-in por provider (flag no config).
- Caveats menores do launcher (opcionais): pid file único (múltiplos bg se sobrescrevem — futuro: pid por
  porta/lockfile); `launch --dry-run` ainda inicia o proxy; `status` logo após bg pode dar "not reachable".
- Fora de escopo v1: retry/round-robin multi-key, rate limit, embeddings, cache, imagem Podman.

## Como rodar os testes
```powershell
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
cd e2e; bun test mock.test.ts
$env:OPENCODE_ZEN_KEY="sk-..."; bun test live-zen.test.ts
```
