# PENDING / status de trabalho

Estado atual do proxy `local-proxy`. Última atualização: 2026-08-18.

## ✅ TODAS AS TASKS CONCLUÍDAS (kanban: backend 001-004, qa 005 — todas `done`)

### Commits
- `3483446` — (001) scaffold: cargo, config YAML/JSON, router, CLI skeleton
- `661821e` — (002) núcleo não-streaming: tradução, handlers, erro formatado, /v1/models, count_tokens
- `86b49a7` — (003) streaming SSE + máquinas de estado nas 3 direções
- `b762f24` — (005) suíte e2e Bun (mock + live opencode-zen), README, config zen, docs
- `2fb1a51` — (004) CLI launcher: serve bg, launch claude/design, status/stop/models

### Estado de verificação
- `cargo test`: 66 unit tests verdes.
- `cargo clippy --all-targets -- -D warnings`: limpo.
- `bun test e2e/mock.test.ts`: 17 testes determinísticos verdes.
- `bun test e2e/live-zen.test.ts`: 6 testes contra opencode-zen real — **environmental skip** enquanto o
  modelo free estiver em `FreeUsageLimitError` (rate limit); validam de verdade quando a quota permitir.

### Nova rodada — config global, default embutido e erros com miette
- Config principal agora vive no diretório de config do usuário (`%APPDATA%\local-proxy\config.yaml`
  no Windows, `~/.config/local-proxy/config.yaml` no Unix); pid + log ficam no mesmo diretório.
- Resolução por precedência: `--config` → `LOCAL_PROXY_CONFIG` → `config.yaml`/`config.json` no cwd
  (override project-local, só se existir) → global default.
- Default global auto-criado a partir de um config embutido, com mensagem indicando onde foi criado
  e para editar e rodar de novo.
- Erros de CLI reportados com **miette** (códigos `config::io`, `config::parse`, `cli::...`,
  contexto-fonte em parse YAML/JSON). O shape `ApiError` para o cliente HTTP (Anthropic/OpenAI)
  permanece inalterado.
- Verificação pendente: `cargo test`, `cargo clippy --all-targets -- -D warnings` e a suíte e2e
  (`bun test e2e/mock.test.ts`, `bun test e2e/live-zen.test.ts`) após a mudança.

### Nova rodada — comando `update`
- Novo comando `local-proxy update [--check] [--force] [--repo owner/repo] [--no-verify]`: consulta o
  último release do GitHub (`gsporto226/local-proxy` ou `$env:LOCAL_PROXY_REPO`), compara a versão com
  a atual, baixa o binário, verifica SHA256 (`sha2`) e faz *stage* como `<bin>.new` ao lado do
  executável em uso, imprimindo o comando manual para concluir (não sobrescreve o binário em execução).
- `--check` apenas informa a versão mais recente; `--force` atualiza mesmo já na latest.
- Erros com códigos `update::unsupported/fetch/no_asset/verify/download/stage` via miette.
- Testes unitários p/ `asset_name`, `parse_version`, `is_newer`, `resolve_repo`.

### Entregas
- Proxy completo (endpoints, tradução A↔O↔Responses, streaming, erros, auth, count_tokens, /v1/models).
- CLI launcher (`serve`, `launch claude|design`, `status`, `stop`, `models`, `update`).
- Suíte e2e em Bun, README, config.example.yaml (com provider `zen`), docs/PLAN.md, docs/PENDING.md.

## Pendências futuras (não-bloqueantes / fora do escopo v1)
- Confirmar `stream_options.include_usage` contra opencode-zen com um 200 limpo (o free rate-limitou antes).
  Se o zen rejeitar, tornar `include_usage` opt-in por provider (flag no config).
- Caveats menores do launcher (opcionais): pid file único (múltiplos bg se sobrescrevem — futuro: pid por
  porta/lockfile); `launch --dry-run` ainda inicia o proxy; `status` logo após bg pode dar "not reachable".
- Fora de escopo v1: retry/round-robin multi-key, rate limit, embeddings, cache, imagem Podman.

## Como rodar os testes
```powershell
cargo test
cd e2e; bun test mock.test.ts
$env:OPENCODE_ZEN_KEY="sk-..."; bun test live-zen.test.ts
```

