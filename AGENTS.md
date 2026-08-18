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
