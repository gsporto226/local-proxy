# local-proxy

Proxy local de tradução **OpenAI ⇄ Anthropic** multi-provider, em Rust (axum). Expõe endpoints
compatíveis com Anthropic Messages API (`/v1/messages` — Claude Code, Claude Design) e OpenAI
Chat Completions / Responses API (`/v1/chat/completions`, `/v1/responses` — Codex e afins), e
traduz entre formatos roteando para qualquer provider compatível com um dos dois.

Referência de design: [ocgo](https://github.com/emanuelcasco/ocgo), generalizado para múltiplos
providers configuráveis, com tradução de erros e `count_tokens` real.

## Recursos

- `/v1/messages`, `/v1/messages/count_tokens`, `/v1/chat/completions`, `/v1/responses`,
  `/v1/models`, `/health`.
- Tradução request/response e **streaming SSE evento-a-evento** nas 3 direções.
- **Catálogo de providers embutido** (`anthropic`, `openai`, `opencode-go`, `zen`, `groq`, `xai`,
  `google`, `deepseek`, `openrouter`); o `config.yaml` só **adiciona** ou **sobrescreve**.
- **Hot-reload**: edições no config e `connect`/MCP aplicam em runtime via file watcher, sem reinício.
- **Auth store** (`auth.json`): chaves separadas do config, como o `/connect` do opencode.
- **Servidor MCP** (`local-proxy mcp`) com tools `connect`/`disconnect`/`providers`/`models(select)`
  para gerenciar chaves e selecionar o modelo ativo sem reiniciar o proxy.
- Roteamento por modelo (rota exata → `provider/model` → prefixo → lista nativa → default).
- Erros do upstream reformatados para o shape do cliente (Anthropic ou OpenAI).
- Auth opcional (`X-API-Key` / `Authorization: Bearer`) e `passthrough_keys`.
- **Estatísticas de uso** (`local-proxy stats`): banco local SQLite (`stats.db`) registra cada request
  (endpoint, provider, model, tokens in/out, latency, streaming, status) e é consultado por janela
  de tempo com resumo por provider e requests recentes.

## Build

```powershell
cargo build
cargo test --all-features   # 91 unit tests
```

## Config (catálogo embutido + overlay)

O proxy embute um catálogo com todos os providers suportados (`src/catalog.yaml`, compilado no
binário). A config do usuário funciona como **overlay**: um provider definido na config com o mesmo
nome **sobrescreve** o do catálogo; um nome novo **adiciona**; rotas e `defaults` definidos na config
vencem os do catálogo. `providers` do catálogo carregam sua chave de `api_key_env` (ex.:
`ANTHROPIC_API_KEY`, `OPENCODE_ZEN_KEY`, `GROQ_API_KEY`, ...).

A config principal vive no diretório de config do usuário:
`%APPDATA%\local-proxy\config.yaml` (Windows) ou `~/.config/local-proxy/config.yaml` (Unix).
Os arquivos de runtime (pid + log + `auth.json`) ficam no mesmo diretório.

Se esse arquivo global não existir, o CLI o cria a partir de um default embutido (mínimo — só
`server:`; o resto vem do catálogo) e imprime uma mensagem dizendo onde foi criado.

Resolução de config (precedência):
1. Flag explícita `--config <path>`.
2. Env var `LOCAL_PROXY_CONFIG`.
3. `config.yaml`/`config.json` no diretório de trabalho (override project-local, só se existir).
4. Global default (o arquivo criado automaticamente).

Exemplo adicionando um provider custom e roteando:

```yaml
providers:
  - name: zen
    base_url: https://opencode.ai/zen
    api_key_env: OPENCODE_ZEN_KEY
    format: openai
    models: [deepseek-v4-flash-free, claude-sonnet-4-5]
routes:
  - model: deepseek-free
    provider: zen
    upstream_model: deepseek-v4-flash-free
defaults:
  provider: anthropic          # fallback final (nome do modelo passa inalterado)
```

Rode (usa a config global; `--config` é só um override):

```powershell
$env:OPENCODE_ZEN_KEY="sk-opencode-zen-..."
cargo run -- serve
```

### Chaves: `connect` / `disconnect`

As chaves ficam em `auth.json` (formato do opencode), nunca no `config.yaml`. `connect` só aceita
providers que **já existem** (no catálogo ou adicionados na config):

```powershell
local-proxy connect opencode-go          # pede a chave oculta
local-proxy connect opencode-go sk-xxx   # ou direto
local-proxy providers                    # lista providers efetivos + key status
local-proxy disconnect opencode-go       # remove a chave
```

Resolução da chave por request: `api_key` inline na config → `auth.json[provider]` → `api_key_env`.

### Modelo ativo (`model` / `models(select)`)

O proxy **nunca usa o modelo pedido pelo harness** (ex.: `ANTHROPIC_MODEL` do Claude Code). Ele
roteia pelo **modelo ativo**: o modelo explicitamente selecionado, senão o **primeiro modelo
disponível de um provider conectado**, senão erro. Um provider está *conectado* quando tem uma chave
resolvível (inline no config → `auth.json` → env var).

Seleção e consulta são feitas pelo CLI **ou** pelo MCP — a mesma lógica (paridade total):

```
local-proxy model                        # modelo ativo (selecionado, senão o primeiro disponível, senão "none")
local-proxy model deepseek-v4-flash      # valida contra providers conectados e define o modelo ativo
local-proxy model clear                  # limpa a seleção (volta ao primeiro disponível)
local-proxy models                       # lista modelos dos providers conectados
```

Via MCP (mesmo comportamento do CLI):

```
mcp connect opencode-go <key>
mcp models                            # lista modelos dos providers conectados
mcp models --select deepseek-v4-flash # define o modelo ativo (valida contra providers conectados)
mcp models --select ""                # limpa a seleção
```

A seleção é persistida em `defaults.active_model` (antes `defaults.model`) no config. Com o campo
setado, o proxy **ignora o modelo pedido pelo harness** e roteia tudo pelo modelo ativo. O file
watcher aplica a mudança **sem reiniciar** o proxy.

### Hot-reload

Qualquer edição no `config.yaml` ou `auth.json` (por CLI, MCP ou manual) é aplicada em runtime por um
file watcher (debounce 300ms) — nada de reinício.

## Servidor MCP

```powershell
local-proxy mcp    # servidor MCP stdio
```

Tools: `connect(provider, key)`, `disconnect(provider)`, `providers()`, `models([select])` — as
tools de modelo/lista delegam exatamente ao CLI (`model`/`models`), então se comportam de forma
idêntica.

### Registrar o MCP nos harnesses (`init`)

`local-proxy init` detecta os harnesses instalados (opencode e/ou Claude Code) e registra o servidor
MCP do local-proxy nos configs deles (`~/.config/opencode/opencode.json` e `~/.claude.json`),
preservando as chaves existentes e fazendo backup em `<path>.bak`:

```powershell
local-proxy init       # pergunta antes de configurar cada harness detectado
local-proxy init --yes # aceita todos os harnesses detectados sem perguntar
```

O `init` **só** registra o MCP — não faz setup de provider nem de modelo (isso fica com
`connect`/`model`).

## Usar com Claude Code

Aponte Claude Code para o proxy:

```powershell
$env:ANTHROPIC_BASE_URL = "http://127.0.0.1:8787"
$env:ANTHROPIC_AUTH_TOKEN = "sk-proxy"   # a api_key do server, ou "unused" se sem auth
claude
```

Claude Code fala `/v1/messages`; o proxy roteia para o provider configurado e traduz (ex.:
`deepseek-free` → deepseek-v4-flash-free via opencode-zen).

## Atualizar (`update`)

Baixa o binário mais recente do último release do GitHub, verifica SHA256 e **aplica no lugar**:
no Linux o swap é atômico (rename POSIX, funciona com o binário em execução); no Windows o
executável em uso é renomeado para um backup e o novo assume o lugar na hora, com a limpeza do
backup feita ao sair e no próximo start. Instalações via `cargo` delegam ao cargo:

```powershell
local-proxy update --check        # só informa a versão mais recente
local-proxy update                # baixa, verifica SHA256 e aplica
local-proxy update --force        # atualiza mesmo se já estiver na latest
local-proxy update --repo dono/repo  # repositório alternativo (ou $env:LOCAL_PROXY_REPO)
local-proxy update --no-verify    # pula a verificação de SHA256
```

O `serve` aceita `--check-update` para avisar no log quando há versão mais recente
(desative com `$env:LOCAL_PROXY_DISABLE_AUTOUPDATE=1`).

## Estatísticas de uso (`stats`)

O proxy registra cada request proxied no banco local `stats.db` (no diretório de config global) —
best-effort: uma falha de escrita é logada e nunca quebra o proxy. Tokens são contabilizados a partir
do `usage` do upstream: nos requests **não-streaming** (do corpo da resposta) e nos **streaming**,
acumulando o `usage` dos frames SSE (chunks OpenAI, `message_start`/`message_delta` Anthropic e
`response.completed` de Responses) — as requisições são registradas quando o fluxo termina.

```powershell
local-proxy stats               # resumo do dia (requests, tokens in/out, latency, erros) + por provider + recentes
local-proxy stats --since week  # dia | week | month | all
local-proxy stats --json        # mesmo relatório em JSON (summary/providers/recent)
```

## Testes

- **Unit (Rust)**: `cargo test --all-features` — traduções, roteamento, erros, máquinas de streaming,
  catálogo/merge, auth, hot-reload, select.
- **e2e (Bun, mock upstream)**: `bun test e2e/mock.test.ts` (ou `bun test` na pasta `e2e/`) —
  sobe o binário + um upstream mock e valida tradução/streaming/auth/erros via HTTP real.
- **e2e live (opencode-zen)**: `$env:OPENCODE_ZEN_KEY=...; bun test e2e/live-zen.test.ts` —
  valida contra o provider real. O upstream free pode limitar por uso (`FreeUsageLimitError`); nesse
  caso os testes reportam *environmental skip* (não são falha do proxy). Inclui também dois testes
  que sobem o **CLI `claude` real** apontado ao proxy (`ANTHROPIC_BASE_URL`, sem auth de cliente — o
  proxy usa a chave configurada do usuário), validando o caminho completo Anthropic → tradução → zen
  com o binário de verdade (`--output-format text` e `stream-json`).

```powershell
# na pasta e2e/
bun install 2>$null; bun test            # mock (determinístico)
$env:OPENCODE_ZEN_KEY="sk-..."; bun test live-zen.test.ts   # live (best-effort)
```

## Estrutura

```
src/
├── main.rs        CLI + boot (erros com miette, códigos config::/cli::)
├── config.rs      Config/Provider/Route/Defaults (YAML/JSON) — overlay, auto-created
├── catalog.rs     catálogo embutido + merge catálogo↔config
├── auth.rs        auth.json (keys) + escrita atômica
├── cli.rs         serve/launch/status/stop/models/model/connect/disconnect/providers/stats/init/mcp/update
├── router.rs      resolve_model → (provider, upstream_model)
├── upstream.rs    chamada HTTP + resolução de chave (inline > auth > env)
├── translate.rs   requests/responses A↔O↔Responses
├── sse.rs         parser de frames SSE
├── streams.rs     máquinas de estado de streaming (3 direções)
├── mcp.rs         servidor MCP stdio (connect/disconnect/providers/models(select))
├── error.rs       ApiError + shape por formato
├── stats.rs       estatísticas locais (SQLite `stats.db`) + `local-proxy stats`
└── handlers.rs    endpoints axum + RuntimeState hot-reload + /v1/models + count_tokens
e2e/               suíte de testes em Bun (mock + live)
```

## Licença

MIT.
