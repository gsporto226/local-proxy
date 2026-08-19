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
- `models(select)` valida e persiste `defaults.active_model` no config; com o campo setado o proxy **ignora
  o modelo do harness** e roteia pelo selecionado (persiste entre reinícios; aplica em runtime via
  watcher).

### Nova rodada — update automático (upgrade estilo opencode)
- `update` detecta o método de instalação (`~/.local/bin` → standalone, `~/.cargo/bin`/`$CARGO_HOME` →
  cargo, senão custom) e aplica de verdade:
  - **Linux**: rename POSIX atômico sobre o binário em execução (o processo velho mantém o inode).
  - **Windows**: o exe em uso é renomeado para `local-proxy.old` (rename é permitido) e o novo assume o
    lugar; o `.old` é apagado por helper `cmd` destacado e limpo também no próximo start
    (`cleanup_stale_backups`).
  - **Cargo**: delega para `cargo install --force local-proxy` (como o opencode delega ao npm).
- Sem permissão no diretório → o binário novo fica staged (`<stem>.new.<pid>[.exe]`) e o erro
  `update::replace` mostra o comando manual.
- `serve --check-update`: avisa no log se houver release mais nova (desativável com
  `$env:LOCAL_PROXY_DISABLE_AUTOUPDATE=1`).
- `cargo test --all-features`: **103** unit tests verdes; fmt/clippy limpos.

### Nova rodada — semântica de modelo + init
- `models`/`model` agora só operam com **providers conectados** (chave resolvível). O proxy **nunca
  usa o modelo pedido pelo harness** — roteia pelo modelo ativo: o selecionado, senão o primeiro de
  um provider conectado, senão erro.
- Campo renomeado: `defaults.model` → `defaults.active_model` (persistido via `model`/MCP
  `models(select)`).
- Novos comandos CLI: `model [<model>]`, `model clear` (mesma lógica do MCP `models(select)`).
- MCP `models`/`select` agora **delegam ao CLI** (`model`/`models`) — paridade total de comportamento
  entre MCP e CLI (mesma validação e mensagens; `models()` lista só modelos de providers conectados).
- `init [--yes]`: detecta harnesses (opencode/claude) e registra o servidor MCP do local-proxy nos
  configs deles (`opencode.json`/`.claude.json`; preserva chaves, backup em `<path>.bak`); só MCP,
  sem setup de provider/modelo.

### Nova rodada — estatísticas locais (`stats`)
- `src/stats.rs`: banco `SQLite` local (`stats.db` no config dir global) registra cada request
  (endpoint, provider, model, tokens in/out, latency, streaming, status, erro). Escrita **best-effort**:
  falha é logada e nunca quebra o proxy.
- Os 3 handlers (`/v1/messages`, `/v1/chat/completions`, `/v1/responses`) capturam por caminho:
  não-streaming registra no corpo da resposta (extrai o `usage` do upstream); erro grava com
  `error=true` e o status real.
- **Streaming**: cada requisição é registrada **quando o SSE termina**. As 3 direções traduzidas
  acumulam o `usage` acumulado nas máquinas de estado (`streams::*_from_*` recebe um
  `StreamCapture`; o driver registra no `finalize`). O **passthrough** de mesmo formato teia os bytes
  originais para o cliente e escaneia os frames SSE (`sse::feed_frames`/`flush_frames`) extraindo o
  `usage` (`translate::usage_from_frame` + `merge_usage` por campo, max).
- `local-proxy stats [--since day|week|month|all] [--json]`: resumo agregado (requests, tokens in/out,
  latency total, taxa de erro) + breakdown por provider + 10 requests mais recentes; `--json` imprime o
  mesmo relatório em JSON (`summary`/`providers`/`recent`).
- `cargo test --all-features`: **113** unit tests verdes (5 novos de stats + 5 de usage SSE);
  fmt/clippy limpos; `bun test e2e/mock.test.ts` 16 verdes.

### Nova rodada — headers por provider (OpenRouter)
- `Provider.headers` (`HashMap` opcional, `#[serde(default)]`): headers estáticos enviados em toda
  request do provider, sobrescrevendo o default de auth/formato quando o nome coincide.
- `upstream::ProviderClient` guarda e aplica os headers em `chat_request` (injetados após o bloco de
  auth, por isso vencem).
- Catálogo: a entrada `openrouter` ganhou os headers recomendados `HTTP-Referer` e `X-Title`
  (roteamento/custo/visibilidade no OpenRouter).
- Testes unit: headers anexados à request (servidor one-shot), override do `Authorization`, e parse
  YAML/round-trip do campo. **116** verdes; fmt/clippy limpos; e2e 16 verdes.

### Estado de verificação (task 010)
- `cargo test --all-features`: **116** unit tests verdes.
- `cargo clippy --all-targets --all-features -- -D warnings`: limpo.
- `cargo fmt --all -- --check`: limpo.
- `bun test e2e/mock.test.ts`: **16** testes determinísticos verdes (suite reescrita para a semântica de
  modelo ativo — cada cenário de roteamento usa o próprio `active_model`).
- `bun test e2e/live-zen.test.ts`: 6 testes contra opencode-zen real — **environmental skip** enquanto o
  modelo free estiver em `FreeUsageLimitError` (rate limit); validam de verdade quando a quota permitir.

### Entregas acumuladas
- Proxy completo (endpoints, tradução A↔O↔Responses, streaming, erros, auth, count_tokens, /v1/models).
- CLI launcher (`serve`, `launch claude|design`, `status`, `stop`, `models`, `connect`, `disconnect`,
  `providers`, `mcp`, `update`).
- Catálogo embutido + overlay, auth.json, hot-reload, servidor MCP com select persistido.
- Suíte e2e em Bun, README, config.example.yaml, docs/PLAN.md, docs/PENDING.md.

## Pendências futuras (não-bloqueantes / fora do escopo v1)
- `models(select)` (persistir `defaults.active_model`) reescreve o config via serde — **comentários/formação
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
