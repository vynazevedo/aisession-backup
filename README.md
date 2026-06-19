# claude-migrate

Backup, restore e migração de sessões do Claude Code entre máquinas, sem perda
e com remapeamento preciso de paths.

[![license](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE)
[![version](https://img.shields.io/badge/version-2.0.0-success?style=flat-square)](Cargo.toml)
[![CI](https://img.shields.io/github/actions/workflow/status/vynazevedo/claude-migrate/ci.yml?style=flat-square&label=CI&branch=main)](https://github.com/vynazevedo/claude-migrate/actions/workflows/ci.yml)
[![rust](https://img.shields.io/badge/rust-1.74%2B-orange?style=flat-square)](https://www.rust-lang.org)
[![Claude Code](https://img.shields.io/badge/Claude%20Code-compatible-8A2BE2?style=flat-square)](https://docs.anthropic.com/claude-code)
[![platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20macOS%20%7C%20Windows-lightgrey?style=flat-square)](#instalação)

Mova suas conversas do Claude Code do Windows para o Mac, do Mac para o Linux,
do WSL para qualquer lugar - ajustando diretórios e nomenclaturas com precisão
absoluta, incluindo caracteres especiais e diferenças de case.

---

## Por que existe

O Claude Code guarda cada sessão em `~/.claude/projects/<encoded>/`, onde
`<encoded>` é o diretório de trabalho com cada caractere nao-alfanumérico
substituido por `-`. Essa transformação **perde informação**:

| Diretório real | Encoded em disco | Pode ter vindo de |
|----------------|------------------|-------------------|
| `/work/claude-migrate` | `-work-claude-migrate` | `claude-migrate`, `claude_migrate`, `claude.migrate` |
| `/home/u/.config` | `-home-u--config` | `.config`, `-config` |
| `/work/alianca_divergente` | `-work-alianca-divergente` | `alianca_divergente`, `alianca-divergente` |

Qualquer ferramenta que reconstrua o path a partir do nome do diretório
corrompe o destino (a versão anterior transformava `claude-migrate` em
`claude/migrate`). `claude-migrate` resolve isso lendo o `cwd` real de dentro
do JSONL da sessão - a unica fonte de verdade - e nunca decodifica o nome do
diretório. No restore, o `cwd` embutido em cada linha do JSONL é reescrito de
forma estruturada para o novo path.

---

## Instalação

### Binário pré-compilado (recomendado)

Cada release publica binários prontos. Baixe o da sua plataforma em
[Releases](https://github.com/vynazevedo/claude-migrate/releases/latest).

| Plataforma | Arquivo |
|------------|---------|
| Linux x86_64 | `claude-migrate-x86_64-unknown-linux-gnu.tar.gz` |
| Linux ARM64 | `claude-migrate-aarch64-unknown-linux-gnu.tar.gz` |
| macOS Apple Silicon | `claude-migrate-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `claude-migrate-x86_64-apple-darwin.tar.gz` |
| Windows x86_64 | `claude-migrate-x86_64-pc-windows-msvc.zip` |

**Linux / macOS** (ajuste o nome do arquivo conforme a sua plataforma):

```bash
# exemplo: Linux x86_64
curl -L -o claude-migrate.tar.gz \
  https://github.com/vynazevedo/claude-migrate/releases/latest/download/claude-migrate-x86_64-unknown-linux-gnu.tar.gz
tar -xzf claude-migrate.tar.gz
chmod +x claude-migrate
sudo mv claude-migrate /usr/local/bin/

claude-migrate version
```

No **macOS**, se o Gatekeeper bloquear o binário (não está assinado), libere com:

```bash
xattr -d com.apple.quarantine claude-migrate
```

**Windows** (PowerShell):

```powershell
Invoke-WebRequest -Uri "https://github.com/vynazevedo/claude-migrate/releases/latest/download/claude-migrate-x86_64-pc-windows-msvc.zip" -OutFile claude-migrate.zip
Expand-Archive claude-migrate.zip -DestinationPath .
# mova claude-migrate.exe para uma pasta no PATH
```

### Compilar do código

Requer Rust 1.74+.

```bash
git clone https://github.com/vynazevedo/claude-migrate
cd claude-migrate
cargo build --release
# binário em target/release/claude-migrate
```

Ou direto via cargo:

```bash
cargo install --git https://github.com/vynazevedo/claude-migrate
```

---

## Quick Start

```bash
# 1. Na máquina de origem: backup interativo
claude-migrate backup -o sessoes.tar.gz

# 2. Transfira sessoes.tar.gz para a outra máquina (scp, pendrive, nuvem)

# 3. Na máquina de destino: preview do que seria restaurado
claude-migrate restore sessoes.tar.gz --dry-run

# 4. Restaure (a TUI sugere o mapeamento de paths conforme o OS)
claude-migrate restore sessoes.tar.gz
```

---

## Como funciona

1. **Descoberta** - varre `~/.claude/projects/**/*.jsonl` e lê o `cwd` real, o
   `sessionId`, o branch git e a versão de dentro de cada JSONL.
2. **Correlação** - reune os artefatos da sessão por prefixo de id:
   `session-env/<id>*` e `todos/<id>*`, inclusive os de sub-agente
   (`<id>-agent-<id>.json`).
3. **Empacotamento** - grava um `.tar.gz` com manifesto v2 e SHA-256 por
   artefato.
4. **Verificação** - no restore, recalcula o hash de cada arquivo e compara com
   o manifesto antes de escrever.
5. **Remapeamento** - aplica o mapeamento de path pelo prefixo mais longo,
   recalcula o diretório encoded de destino e reescreve o `cwd` linha a linha
   dentro do JSONL.

---

## Comandos

| Comando | Descrição |
|---------|-----------|
| `backup` | Seleciona sessões na TUI e gera o `.tar.gz` |
| `restore <arquivo>` | Restaura sessões com remapeamento de paths |
| `list [arquivo]` | Lista sessões locais, ou dentro de um backup |
| `version` | Mostra a versão |

### backup

```bash
claude-migrate backup                 # TUI, nome de arquivo automático
claude-migrate backup -o meu.tar.gz   # saída específica
claude-migrate backup --full          # inclui shell-snapshots, file-history, plans
claude-migrate backup --config        # inclui config global (settings, plugins, MCP)
claude-migrate backup --all           # sessões + extras + config
```

| Flag | Descrição |
|------|-----------|
| `-o, --output` | Caminho do arquivo de saída |
| `--full` | Inclui `shell-snapshots`, `file-history`, `plans` |
| `--config` | Inclui config global: settings, `CLAUDE.md`, plugins, MCP servers |
| `--include-credentials` | Com `--config`, inclui também credenciais de login (sensível) |
| `--all` | Atalho para `--full --config` |

Controles da TUI: setas ou `j`/`k` navegar, `space` selecionar, `a` todas,
`n` nenhuma, `/` filtrar, `b`/`enter` criar backup, `q` sair.

### restore

```bash
claude-migrate restore meu.tar.gz                       # TUI interativa
claude-migrate restore meu.tar.gz --dry-run             # preview, sem escrever
claude-migrate restore meu.tar.gz -m "/home/u:/Users/u" # mapeamento direto
claude-migrate restore meu.tar.gz -f                    # sem TUI, sem mapeamento
```

| Flag | Descrição |
|------|-----------|
| `-m, --mappings` | Mapeamentos `origem:destino` separados por vírgula |
| `-d, --dest` | Diretório Claude destino (default `~/.claude`) |
| `--dry-run` | Mostra o que seria feito sem escrever |
| `-f, --force` | Pula a TUI (restore nao-interativo) |
| `--rewrite-tool-paths` | Reescreve tambem paths absolutos em chamadas de ferramenta |
| `--restore-history` | Acrescenta entradas ao `history.jsonl` |
| `--no-verify-checksum` | Não verifica checksum antes de restaurar |
| `--config` | Aplica o pacote de config global presente no backup |
| `--overwrite-config` | Com `--config`, sobrescreve config e MCP existentes |

---

## Mapeamento de paths

Formato `origem:destino`, aplicado pelo prefixo mais longo que casa:

```bash
# Linux/WSL -> macOS
-m "/home/user:/Users/user"

# WSL mount -> macOS
-m "/mnt/e/workspace:/Users/user/workspace"

# Múltiplos
-m "/home/u/work:/Users/u/work,/home/u/pessoal:/Users/u/pessoal"
```

Na TUI os roots de projeto são detectados automaticamente e o destino é
sugerido conforme o OS de origem e destino. Em destinos case-insensitive
(macOS APFS, Windows), colisões de case são detectadas e avisadas antes de
sobrescrever.

---

## Migrando para um diretório diferente

O Claude Code indexa as sessões pelo diretório de trabalho: `claude --resume`
lista apenas o que está em `~/.claude/projects/<encode(cwd_atual)>/`. Logo,
mover uma sessão para um path diferente funciona desde que você forneça o
mapeamento - a ferramenta recalcula o diretório encoded de destino e reescreve
o `cwd` dentro do JSONL.

```bash
# exportou em /home/vinicius/projects/app (Linux)
# no Mac o projeto vive em /Users/vini/dev/app
claude-migrate restore sessoes.tar.gz \
  -m "/home/vinicius/projects:/Users/vini/dev"

cd /Users/vini/dev/app
claude --resume        # a sessão migrada aparece
```

Pontos importantes:

- Forneça o mapeamento sempre que o diretório mudar; sem ele a sessão cai sob o
  path antigo.
- Rode `claude --resume` de dentro do novo diretório.
- Para continuar trabalhando (não só ler o histórico), tenha o código do
  projeto no novo path.
- Use `--dry-run` para conferir o `encoded:` de destino antes de escrever.
- Re-restaurar com outro mapeamento é seguro: cria a cópia no novo encoded e
  pula a antiga, sem perder nada.

---

## Configuração global (MCP, settings)

O objetivo da ferramenta é trocar de computador sem recomeçar. Com `--config`,
o backup também carrega sua configuração global:

- `settings.json`, `settings.local.json`, `CLAUDE.md`
- arquivos de config dos plugins
- os servidores MCP registrados (`mcpServers` do `~/.claude.json`)

```bash
# origem
claude-migrate backup --all -o tudo.tar.gz

# destino: aplica sessões + extras + config
claude-migrate restore tudo.tar.gz --config
```

No restore, os servidores MCP são **mesclados** no `~/.claude.json` do destino
sem apagar o que já existe (use `--overwrite-config` para sobrescrever). Os
arquivos de config só são escritos se ainda não existirem, salvo
`--overwrite-config`.

Atenção a segredos:

- Definições de MCP podem conter API keys; um backup com `--config` passa a
  conter segredos. Transfira e armazene o arquivo com cuidado.
- As credenciais de login (`.credentials.json`, `oauthAccount`) ficam **de
  fora por padrão**. Inclua apenas com `--include-credentials`, ciente de que
  isso carrega o seu login do Claude.

---

## Estrutura do backup

```
backup.tar.gz
├── manifest.json
├── sessions/
│   └── <session-id>/
│       ├── <session-id>.jsonl
│       ├── env/<arquivo>.json
│       └── todos/<arquivo>.json
├── extras/                      (apenas com --full)
│   ├── shell-snapshots/...
│   ├── file-history/...
│   └── plans/...
└── config/                      (apenas com --config)
    ├── settings.json
    ├── CLAUDE.md
    ├── plugins/...
    └── mcp-servers.json
```

O `manifest.json` (schema v2) registra OS de origem, versão do Claude Code, e
para cada sessão o `cwd` real e o SHA-256 de cada artefato.

---

## Desenvolvimento

```bash
cargo test                  # 23 testes (unit + roundtrip de integração)
cargo clippy --all-targets  # lint
cargo fmt                   # formatação
```

Layout:

| Módulo | Responsabilidade |
|--------|------------------|
| `claudepath` | Encode canônico de path (sem decode, por ser lossy) |
| `discover` | Descoberta de sessões via `cwd` do JSONL |
| `remap` | Mapeamento de path e rewrite estruturado do JSONL |
| `backup` | Empacotamento `.tar.gz` + checksum |
| `restore` | Restore + verificação + colisão de case |
| `cli` | Interface de linha de comando (clap) |
| `tui` | Interface interativa (ratatui) |

---

## Troubleshooting

- **Sessão nao aparece após restore** - confirme que rodou `claude --resume` no
  diretório de destino correto, e cheque `~/.claude/projects/` pelo nome
  encoded mostrado no preview.
- **Sessão pulada (skipped)** - já existe no destino; o restore nunca
  sobrescreve. Use `--dry-run` para inspecionar.
- **Falha de checksum** - o backup está corrompido ou truncado; gere de novo.
  Para forçar, use `--no-verify-checksum`.

---

## Roadmap

- v2.0 (atual) - descoberta via JSONL, manifesto v2 com checksum, remapeamento
  estruturado de `cwd`, backup `--full`, TUI, compatibilidade de leitura v1.
- v2.1 - publicação em crates.io e Homebrew, instalação por binário.
- v2.2 - rewrite opcional de paths em todos os artefatos com relatório
  detalhado de diff.

---

## Licença e suporte

MIT - veja [LICENSE](LICENSE).

- Issues: https://github.com/vynazevedo/claude-migrate/issues
- Repositório: https://github.com/vynazevedo/claude-migrate
