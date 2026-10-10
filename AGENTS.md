# AGENTS.md

## Antes de commitar: rode exatamente o que o CI roda

Para evitar CI vermelho, valide localmente **todos** os comandos do workflow
`.github/workflows/ci.yml` antes de push. O CI roda `cargo fmt --all`,
`cargo clippy --all-targets --all-features -- -D warnings` e `cargo test
--all-features` em `ubuntu-latest` e `windows-latest`, mais a suíte e2e de
Bun.

### 1. Formatação (estrito)
```powershell
cargo fmt --all -- --check
```
Se falhar, rode `cargo fmt --all` e confira o diff antes de commitar.

### 2. Lint (estrito: `-D warnings` + pedantic + nursery)
```powershell
cargo clippy --all-targets --all-features -- -D warnings
```
Não commite com warnings. `--all-features` importa: testa com todas as features.

### 3. Testes unitários
```powershell
cargo test --all-features
```

### 4. Suíte e2e (mock determinístico)
```powershell
# na pasta e2e/
cargo build --manifest-path ../Cargo.toml
bun install
bun test mock.test.ts
```
O e2e usa o binário debug; o `cargo build` acima garante que ele existe.

### 5. cargo-audit
`cargo-audit` (job `cargo-audit`) exige rede e instalação do
`rustsec/audit-check`; rode `cargo audit` se tiver disponível, mas não é
bloqueante localmente.

### Regras
- Todos os 4 primeiros itens acima devem passar **antes** de `git commit` /
  `git push`. Se qualquer um falhar, corrija e re-verifique os quatro.
- O projeto usa `missing_docs = "deny"`: todo item público precisa de `///`.
- Não rode só `cargo test` — o CI reprova em `cargo fmt`/`cargo clippy` mesmo
  com os testes verdes (foi o que causou CI vermelho em `cc1b86c`).
- O build compila SQLCipher com OpenSSL vendorizado: `perl` precisa estar no
  PATH (Strawberry Perl no Windows). No CI o Perl já vem na imagem. No Linux,
  as bindings do Secret Service precisam de `libdbus-1-dev` e `pkg-config`.

### Testes e2e nunca tocam o store real

O store de credenciais vive em `<config dir>/accounts.db` com a chave no cofre
do SO (`%APPDATA%\local-proxy\config\accounts.db` no Windows). Um `auth.json`
legado ainda pode existir e é migrado (renomeado para `auth.json.migrated`) na
primeira leitura — um teste que rode contra o diretório real mexe nas
credenciais do usuário.

Todo suite que precise semear credenciais usa `isolatedConfigDir()` de
`e2e/helpers.ts`, que cria um diretório temporário e devolve o env
`LOCAL_PROXY_CONFIG_DIR` apontando para ele (o proxy e os subcomandos da CLI
respeitam essa variável). Passe esse env ao `startProxy` e escreva o
`auth.json` dentro do diretório retornado:

```ts
const { dir, env } = isolatedConfigDir();
writeFileSync(authStorePath(dir), JSON.stringify({ meu_provider: { type: "api", key: "k" } }));
const proxy = await startProxy(cfg, undefined, env);
```

Nunca faça backup/restore do arquivo real: uma restauração que não roda é uma
perda silenciosa de credenciais. O prefixo do diretório temporário não pode
começar com `local-proxy-e2e-` (o `stopProxy` varre temporários com esse
prefixo e apagaria o store de outro suite).

### Testes unitários (Rust) nunca tocam o store real

O mesmo vale para `cargo test`. Todo teste que leia ou escreva credenciais pega
o `TEST_STATE_LOCK` e aponta `LOCAL_PROXY_CONFIG_DIR` para um diretório
temporário enquanto roda:

```rust
let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
std::env::set_var("LOCAL_PROXY_CONFIG_DIR", &dir);
// ... ports.credentials / build_runtime_state / settings::* / etc ...
std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
```

O lock serializa todos os testes que mexem no env (adapters e application).
As credenciais passam pelo port `CredentialStore`; testes de lógica pura podem
usar fakes (`application::testing`) em vez do store real. Como defesa extra,
`with_db` em `adapters/outbound/credential_store.rs` falha
em `cfg(test)` sem `LOCAL_PROXY_CONFIG_DIR`: um teste esquecido falha alto em
vez de migrar o store real — foi assim que o `auth.json` do usuário foi
migrado por engano em 06/10/2026 (teste
`rebuild_merges_catalog_with_overlay_and_reapplies`, que chamava
`build_runtime_state` sem isolar o diretório).
