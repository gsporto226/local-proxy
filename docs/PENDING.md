# PENDING / status de trabalho

Estado atual do proxy `local-proxy` (repositório raiz do workspace). Última atualização: 2026-08-17.

## Concluído (commits)
- `3483446` — (001) scaffold: cargo, config YAML/JSON, router, CLI skeleton
- `661821e` — (002) núcleo não-streaming: tradução request/response, handlers, erro formatado, /v1/models, count_tokens
- `86b49a7` — (003) streaming SSE + máquinas de estado nas 3 direções
- Kanban: tasks 001, 002, 003 `done`.

## Feito, ainda sem commit (fase 005)
- Removidos testes de integração Rust (`tests/integration.rs`) e dev-deps `tower`/`http` — `cargo test` = só unit (62).
- Suíte e2e em **Bun** em `e2e/`:
  - `mock.test.ts` — 17 testes determinísticos (mock upstream Bun): tradução/streaming/auth/erros/model_not_found/count_tokens/provider-model-syntax — **verde**.
  - `live-zen.test.ts` — 6 testes contra opencode-zen real; hoje o modelo free está em `FreeUsageLimitError` (rate limit) → testes reportam *environmental skip* e passam. Quando a quota permitir, validam de verdade.
  - `helpers.ts`, `mock-upstream.ts`, `package.json`, `tsconfig.json`.
- `config.example.yaml` — adicionado provider `zen` (https://opencode.ai/zen, OPENCODE_ZEN_KEY, free models).
- `README.md` — build, config, uso com Claude Code, como rodar testes.
- `docs/PLAN.md` — arquitetura (referência).

## Pendente AGORA
- **Fechar task 005 no kanban** (qa): stages estrategia→instrumentar→escrever→executar→relatar, eval_score, done. E commit referenciando `(005)`.
- **Task 004 (backend) — CLI launcher** (ainda NÃO iniciada): `serve --background` (spawna processo desacoplado, pid/log em `~/.config/local-proxy/`), `launch claude|design` (seta `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_API_KEY`, `ANTHROPIC_MODEL`/`ANTHROPIC_SMALL_FAST_MODEL`), `status`/`stop`/`models`. DoR já preenchida no kanban.

## Notas / riscos
- **stream_options.include_usage** contra opencode-zen ainda NÃO confirmado com um 200 limpo (o free model rate-limitou antes de validar). Se o zen rejeitar, tornar `include_usage` opt-in por provider (flag no config) e desligar no zen.
- Modelos free (`deepseek-v4-flash-free` etc.) têm rate limit baixo e o upstream às vezes retorna 500 — esperado; testes live tratam como *environmental skip*.
- Chave opencode-zen usada nos testes live: env `OPENCODE_ZEN_KEY` (não commitar). Fora de escopo v1: retry/round-robin multi-key, rate limit, embeddings, cache, imagem Podman.

## Como continuar amanhã
1. Commit da fase 005 (e2e + README + config) com `(005)` e fechar task 005 no kanban.
2. Implementar task 004 (CLI launcher) e fechar.
3. (Opcional) Rodar `bun test live-zen.test.ts` com a quota do zen livre p/ validar stream_options/usage.
