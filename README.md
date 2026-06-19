# claude-migrate

Ferramenta de linha de comando para backup, restore e migração de sessões do
Claude Code entre máquinas (Windows, macOS, Linux/WSL), com remapeamento de
paths sem perda.

## Por que existe

O Claude Code guarda cada sessão em `~/.claude/projects/<encoded>/`, onde
`<encoded>` é o diretório de trabalho com cada caractere não-alfanumérico
substituido por `-`. Essa transformação é irreversivel: `claude-migrate`,
`claude_migrate` e `claude.migrate` produzem todos o mesmo `claude-migrate`.
Qualquer ferramenta que tente reconstruir o path a partir do nome do diretório
corrompe o destino.

`claude-migrate` resolve isso lendo o `cwd` real de dentro do JSONL da sessão -
a única fonte de verdade - e nunca decodifica o nome do diretório. No restore,
o `cwd` embutido em cada linha do JSONL é reescrito de forma estruturada para o
novo path.

## Features

- Descoberta de sessões pelo `cwd` real lido do JSONL (preciso com `-`, `_`,
  `.`, espaços e case).
- Backup portável `.tar.gz` com manifesto v2 e checksum SHA-256 por artefato.
- Inclui todos os artefatos correlatos: sessão, `session-env` e `todos`,
  inclusive os de sub-agente (`<id>-agent-<id>.json`).
- Restore com remapeamento de paths por prefixo mais longo.
- Reescrita estruturada do `cwd` dentro do JSONL (e, opcional, dos paths em
  chamadas de ferramenta).
- Verificação de checksum antes de restaurar.
- Detecção de colisão de case em destinos case-insensitive (macOS, Windows).
- Interface interativa (TUI) para seleção e mapeamento.
- Dry-run e detecção de duplicatas.
- Compatibilidade de leitura com backups v1 (formato Go legado).

## Instalação

```bash
cargo build --release
# binário em target/release/claude-migrate
```

## Comandos

### backup

Abre a TUI para selecionar quais sessões exportar.

```bash
claude-migrate backup
claude-migrate backup -o meu-backup.tar.gz
```

Controles: setas ou `j`/`k` navegar, `space` selecionar, `a` todas, `n`
nenhuma, `/` filtrar, `b`/`enter` criar backup, `q` sair.

### restore

```bash
# Interativo (TUI: seleciona, mapeia paths, preview, executa)
claude-migrate restore meu-backup.tar.gz

# Preview sem escrever
claude-migrate restore meu-backup.tar.gz --dry-run

# Automático com mapeamentos
claude-migrate restore meu-backup.tar.gz -m "/home/user:/Users/user"

# Não-interativo sem mapeamento
claude-migrate restore meu-backup.tar.gz -f
```

Flags:

| Flag | Descrição |
|------|-----------|
| `-m, --mappings` | Mapeamentos `origem:destino` separados por vírgula |
| `-d, --dest` | Diretório Claude destino (default `~/.claude`) |
| `--dry-run` | Mostra o que seria feito sem escrever |
| `-f, --force` | Pula a TUI (restore não-interativo) |
| `--rewrite-tool-paths` | Reescreve tambem paths absolutos embutidos em tool calls |
| `--restore-history` | Acrescenta entradas ao `history.jsonl` |
| `--no-verify-checksum` | Não verifica checksum antes de restaurar |

### list

```bash
claude-migrate list                 # sessões locais
claude-migrate list meu-backup.tar.gz
```

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

Na TUI, os roots de projeto são detectados automaticamente e sugestões de
destino são geradas conforme o OS de origem e destino.

## Estrutura do backup (v2)

```
backup.tar.gz
├── manifest.json
└── sessions/
    └── <session-id>/
        ├── <session-id>.jsonl
        ├── env/<arquivo>.json
        └── todos/<arquivo>.json
```

O `manifest.json` registra `schema_version`, OS de origem, versão do Claude
Code, e para cada sessão o `cwd` real e o SHA-256 de cada artefato.

## Build multiplataforma

Recomenda-se compilar nativamente em cada OS (via CI). Cross-compile a partir
do Linux:

```bash
# Windows
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

## Desenvolvimento

```bash
cargo test
cargo clippy --all-targets
```

## Fora de escopo

Não são incluidos no backup: `shell-snapshots/`, `file-history/`, `plans/` e
arquivos de configuração. O foco é a sessão e seus artefatos diretos.
