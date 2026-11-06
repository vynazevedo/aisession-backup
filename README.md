# aisession-backup

Backup, restore e migração de sessões e configuração de ferramentas de IA para
terminal entre máquinas, com plano prévio, rollback e remapeamento preciso de
paths.

AI session backup and migration for terminal coding tools: Claude Code, Codex,
Grok, Gemini, Antigravity, Maestri and OpenCode.

[![license](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE)
[![version](https://img.shields.io/badge/version-3.0.0-success?style=flat-square)](Cargo.toml)
[![CI](https://img.shields.io/github/actions/workflow/status/vynazevedo/aisession-backup/ci.yml?style=flat-square&label=CI&branch=main)](https://github.com/vynazevedo/aisession-backup/actions/workflows/ci.yml)
[![rust](https://img.shields.io/badge/rust-1.85%2B-orange?style=flat-square)](https://www.rust-lang.org)
[![platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20macOS%20%7C%20Windows-lightgrey?style=flat-square)](#instalação)

---

## Provedores suportados

| Provedor | Id | O que entra no backup | Paths remapeados no restore |
|----------|----|-----------------------|-----------------------------|
| Claude Code | `claude` | Sessões, subagentes, memória, extras e config | `cwd` de cada linha do JSONL e diretório encoded |
| Codex CLI | `codex` | Rollouts e `config.toml` | `cwd` e workspaces dos rollouts, projetos do `config.toml` |
| Grok CLI | `grok` | Sessões por diretório | Diretório da sessão, `info.cwd` e `git_root_dir` |
| Gemini CLI | `gemini` | Chats, `projects.json` e `trustedFolders.json` | `.project_root`, `projects.json` e `trustedFolders.json` |
| Antigravity CLI | `agy` | Conversas, `brain/`, histórico e settings | `history.jsonl` e `trustedWorkspaces` |
| Maestri | `maestri` | Workspaces, notas, preferências e layout | `workingDirectory` dos workspaces e terminais |
| OpenCode | `opencode` | `storage/` | Nenhum: cópia literal, formato não validado |

Limitações conhecidas:

- Conversas do Antigravity (`conversations/*.db`) e bancos SQLite do Codex são
  copiados sem reescrita interna. Eles continuam citando os paths antigos, e o
  índice de threads do Codex não é atualizado.
- Diretórios do Gemini identificados por hash continuam sob o hash antigo.
- Um banco SQLite em uso pode ser copiado em estado intermediário. Para esses
  arquivos, feche a ferramenta antes do backup.

---

## Instalação

### Binário pré-compilado

Baixe o arquivo da sua plataforma em
[Releases](https://github.com/vynazevedo/aisession-backup/releases/latest).

| Plataforma | Arquivo |
|------------|---------|
| Linux x86_64 | `aisession-backup-x86_64-unknown-linux-gnu.tar.gz` |
| Linux ARM64 | `aisession-backup-aarch64-unknown-linux-gnu.tar.gz` |
| macOS Apple Silicon | `aisession-backup-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `aisession-backup-x86_64-apple-darwin.tar.gz` |
| Windows x86_64 | `aisession-backup-x86_64-pc-windows-msvc.zip` |

```bash
curl -L -o aisession-backup.tar.gz \
  https://github.com/vynazevedo/aisession-backup/releases/latest/download/aisession-backup-x86_64-unknown-linux-gnu.tar.gz
tar -xzf aisession-backup.tar.gz
chmod +x aisession-backup
sudo mv aisession-backup /usr/local/bin/
```

No macOS, se o Gatekeeper bloquear o binário não assinado:

```bash
xattr -d com.apple.quarantine aisession-backup
```

### Compilar do código

Requer Rust 1.85 ou superior.

```bash
cargo install --git https://github.com/vynazevedo/aisession-backup
```

---

## Início rápido

```bash
# Na origem: veja o que foi detectado e procure segredos
aisession-backup doctor
aisession-backup audit --provider all

# Confira o plano e gere um backup criptografado
aisession-backup backup --provider all --encrypt -o tudo.tar.gz.age --dry-run
aisession-backup backup --provider all --encrypt -o tudo.tar.gz.age

# No destino: confira o plano e restaure
aisession-backup verify tudo.tar.gz.age
aisession-backup restore tudo.tar.gz.age -m "/home/u:/Users/u" --dry-run
aisession-backup restore tudo.tar.gz.age -m "/home/u:/Users/u"

# Se precisar desfazer
aisession-backup history
aisession-backup rollback <ID>
```

Sem argumentos em um terminal, `aisession-backup` abre a interface interativa.
Sem terminal, mostra a ajuda.

---

## Comandos

| Comando | Descrição |
|---------|-----------|
| `doctor` | Provedores detectados, paths, tamanhos e processos em execução |
| `scan` | Inventário de sessões, configs e workspaces |
| `audit` | Procura tokens nos arquivos selecionados, sem exibir os valores |
| `backup` | Gera o arquivo de backup |
| `verify <arquivo>` | Confere integridade, checksums e segurança das entradas |
| `list [arquivo]` | Lista sessões locais ou de um arquivo |
| `restore <arquivo>` | Restaura sessões e itens de provedores |
| `history` | Lista os restores registrados no journal |
| `rollback <id>` | Desfaz um restore |
| `completions <shell>` | Gera autocomplete para bash, zsh, fish, elvish ou powershell |
| `version` | Mostra a versão |

`doctor`, `scan`, `audit`, `backup`, `verify`, `list`, `restore`, `history` e
`rollback` aceitam `--json`.

### backup

Sem `--provider`, o backup considera só o Claude Code.

| Flag | Descrição |
|------|-----------|
| `-p, --provider` | Lista separada por vírgula, ou `all` |
| `--match <SUBSTR>` | Só itens cujo `cwd` contém `SUBSTR` |
| `--full` | Inclui diretórios auxiliares do Claude |
| `--config` | Inclui configuração global |
| `--all` | Atalho para `--full --config` |
| `--include-state` | Inclui bancos de estado e histórico dos provedores |
| `--include-credentials` | Inclui credenciais de login. Exige `--encrypt` |
| `--encrypt` | Criptografa com senha usando age |
| `-o, --output` | Arquivo de saída |
| `--overwrite` | Permite substituir um arquivo de saída existente |
| `--dry-run` | Mostra o plano e sai sem escrever |
| `-y, --yes` | Dispensa a confirmação |

### restore

| Flag | Descrição |
|------|-----------|
| `-d, --dest` | Diretório de destino do Claude |
| `--provider-home <ID=PATH>` | Destino de um provedor. Pode ser repetida |
| `-m, --mappings` | Pares `origem:destino` separados por vírgula |
| `-p, --provider` | Restaura só os provedores indicados |
| `--dry-run` | Mostra o plano e sai sem escrever |
| `-y, --yes` | Dispensa a confirmação |
| `--config` | Aplica a configuração global do arquivo |
| `--overwrite` | Permite sobrescrever arquivos e entradas de config |
| `--allow-exec-config` | Permite config capaz de executar código |
| `--allow-credentials` | Permite restaurar credenciais |
| `--allow-running` | Permite restaurar com o provedor em execução |
| `--rewrite-tool-paths` | Reescreve paths dentro de chamadas de ferramenta |
| `--restore-history` | Acrescenta as sessões ao `history.jsonl` |
| `--no-verify-checksum` | Ignora a verificação prévia do arquivo |

Sem `--dest`, cada provedor vai para o diretório real dele. Com `--dest`, nenhum
outro provedor usa o diretório real por padrão: o plano recusa os que não têm
`--provider-home`, a menos que `--provider` os deixe de fora.

```bash
# Restaura tudo em um diretório de teste
aisession-backup restore tudo.tar.gz.age \
  --dest /tmp/teste/.claude \
  --provider-home codex=/tmp/teste/.codex \
  --provider-home maestri=/tmp/teste/.maestri \
  --dry-run

# Restaura só o Claude em outro diretório
aisession-backup restore tudo.tar.gz.age --dest /tmp/teste/.claude --provider claude
```

### Interface interativa

Na tela inicial, escolha provedores e itens e tecle `b` para o backup ou `r`
para o restore. Toda escrita passa por uma tela com o plano real.

No restore, o plano é recalculado a cada mudança, e o plano confirmado é o que
será executado:

| Tecla | Ação na tela de plano |
|-------|-----------------------|
| `enter` ou `y` | Executa. Fica bloqueado enquanto houver `REFUSE` |
| `r` | Liga ou desliga `allow-running` |
| `x` | Liga ou desliga `exec-config` |
| `c` | Liga ou desliga `credentials` |
| `o` | Liga ou desliga `overwrite` |
| `esc` | Volta ao mapeamento |
| `q` | Sai sem escrever |

O backup criptografado e a seleção de credenciais estão disponíveis apenas na
linha de comando. Na interface, o restore seleciona sessões do Claude
individualmente, e os itens dos outros provedores aparecem no plano como um
conjunto.

---

## Segurança

O fluxo prioriza não perder dados e não executar nada inesperado:

- **Plano primeiro.** `--dry-run` mostra o plano que a execução real usa. Cada
  operação traz ação (`create`, `overwrite`, `merge`, `skip`, `REFUSE`) e risco
  (`normal`, `SENSITIVE`, `EXECUTES_CODE`).
- **Confirmação.** Sem terminal e sem `--yes`, nada é escrito e o código de
  saída é 3.
- **Rollback.** Cada restore grava um journal com o conteúdo anterior.
- **Escrita atômica e privada.** Arquivos com modo 0600 e diretórios com 0700
  em Unix.
- **Arquivo hostil.** Entradas com path traversal, symlinks abaixo do destino,
  tipos especiais e tamanhos acima dos limites são rejeitados.
- **Credenciais.** Ficam de fora por padrão, entram só com `--encrypt` e voltam
  só com `--allow-credentials`.
- **Config executável.** Hooks e comandos de terminal são pulados sem
  `--allow-exec-config`. O plano informa o motivo.
- **Saída limpa.** Sequências de controle vindas de manifestos são removidas
  antes da exibição.
- **Senha fora do argv.** Use o prompt ou `AISESSION_BACKUP_PASSPHRASE`.

O que essas proteções não garantem:

- Os checksums de um arquivo sem criptografia detectam corrupção, mas não
  adulteração. Quem altera o arquivo pode recalculá-los. Só restaure arquivos
  de origem confiável.
- O detector de segredos usa padrões conhecidos e pode deixar passar outros
  formatos. Sessões costumam conter segredos colados em conversas, então prefira
  `--encrypt`.
- A detecção de provedor em execução compara nomes de processo e funciona apenas
  no Linux. Em outros sistemas, feche as ferramentas antes do restore.

### Códigos de saída

| Código | Significado |
|--------|-------------|
| 0 | Sucesso |
| 1 | Erro |
| 2 | Uso incorreto |
| 3 | Confirmação necessária ou recusada |
| 4 | Verificação do arquivo falhou |
| 5 | O plano contém recusas |

### Variáveis de ambiente

| Variável | Uso |
|----------|-----|
| `CLAUDE_CONFIG_DIR` | Diretório do Claude |
| `CODEX_HOME` | Diretório do Codex |
| `MAESTRI_HOME` | Diretório do Maestri |
| `AISESSION_BACKUP_PASSPHRASE` | Senha em modo não interativo |
| `AISESSION_BACKUP_DATA_DIR` | Base do journal de restores |
| `AISESSION_BACKUP_MAX_ARCHIVE_BYTES` | Limite do tamanho total extraído |
| `NO_COLOR` | Desativa cores |

---

## Atualizando da v2

O projeto se chamava `claude-migrate`. Na v3:

- **Binário.** O comando agora é `aisession-backup`. Atualize scripts e aliases, e
  gere de novo o autocomplete com `completions`.
- **Variáveis.** `CLAUDE_MIGRATE_*` continuam aceitas quando a equivalente
  `AISESSION_BACKUP_*` não está definida.
- **Journal.** `history` e `rollback` continuam lendo os restores do diretório
  antigo. Os novos ficam em `aisession-backup/journal`.
- **Arquivos.** Backups gerados pela v2 podem ser verificados e restaurados. Novos
  backups usam o schema v3, que a v2 não lê.
- **Compilação.** O mínimo agora é Rust 1.85.

---

## Mapeamento de paths

O formato é `origem:destino`, aplicado pelo prefixo mais longo que casa em uma
fronteira de diretório. Entre Windows e POSIX, os separadores do restante do
path são convertidos.

```bash
-m "/home/user:/Users/user"
-m "/mnt/e/workspace:/Users/user/workspace"
-m "C:\Users\user:/home/user"
-m "/home/u/work:/Users/u/work,/home/u/pessoal:/Users/u/pessoal"
```

O Claude Code localiza sessões pelo diretório atual. Depois de migrar, rode
`claude --resume` de dentro do novo diretório.

O path original vem do `cwd` gravado em cada sessão, nunca do nome do diretório
encoded, porque essa codificação perde informação: `/work/a_b` e `/work/a-b`
geram o mesmo nome.

---

## Formato do arquivo

```
backup.tar.gz
├── manifest.json
├── sessions/<session-id>/...
├── extras/...
├── config/...
└── providers/<id>/...
```

O manifesto (schema v3) registra origem e SHA-256 de cada arquivo. Arquivos
criptografados são o mesmo `tar.gz` dentro de um envelope age. Arquivos v2
continuam legíveis. O código ainda aceita o schema v1, mas essa leitura não foi
testada com um arquivo real daquela versão.

---

## Desenvolvimento

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

O CI roda os testes em Linux, macOS e Windows. O teste de provedor em execução só
existe no Linux, a única plataforma com essa detecção.

| Módulo | Responsabilidade |
|--------|------------------|
| `engine` | Planejamento, backup, restore, rollback e auditoria |
| `archive` | Leitura e escrita do arquivo, limites e verificação |
| `crypto` | Envelope age com senha |
| `journal` | Registro de restores e pré-imagens para rollback |
| `safety` | Validação de paths e escrita atômica |
| `secrets` | Detecção de tokens em streaming |
| `plan` | Ações, riscos e resumo do plano |
| `provider` | Adaptadores de cada ferramenta |
| `remap` | Mapeamento de paths e reescrita de JSONL |
| `discover`, `backup`, `config`, `claudepath`, `model` | Domínio do Claude Code e manifesto |
| `cli`, `ui`, `tui` | Linha de comando, formatação e interface interativa |

---

## Solução de problemas

- **`provider may be running`**: feche a ferramenta. Se aceitar o risco, use
  `--allow-running` ou a tecla `r` na interface.
- **`provider destination not specified`**: há itens desse provedor no arquivo e
  você usou `--dest`. Informe `--provider-home` ou filtre com `--provider`.
- **Item `skip`**: o motivo aparece no plano. Normalmente falta uma permissão
  explícita ou o arquivo já existe.
- **Sessão não aparece após o restore**: rode `claude --resume` no diretório
  mapeado e confira o destino com `--dry-run`.
- **`verify` retorna 4**: o arquivo está corrompido, truncado, ou a senha está
  errada.

---

## Licença

MIT. Veja [LICENSE](LICENSE).

- Issues: https://github.com/vynazevedo/aisession-backup/issues
