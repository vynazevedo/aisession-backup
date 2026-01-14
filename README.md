# Claude Code Session Migrator v2

Ferramenta interativa para backup e restore de sessões do Claude Code entre máquinas.

## Features

- **Interface interativa (TUI)** para selecionar sessões
- **Backup portável** em arquivo `.tar.gz` com checksum SHA256
- **Restore com remapeamento de paths** (WSL → macOS, etc)
- **Dry-run** para preview antes de restaurar
- **Detecção de duplicatas** para evitar sobrescrever sessões
- **Listagem** de sessões locais ou em backups
- **Preview** das mensagens de cada sessão

## Instalação

```bash
cd claude-migrate
go mod download
go build -o claude-migrate .

# Opcional: instalar globalmente
sudo mv claude-migrate /usr/local/bin/
```

## Comandos

### `version` - Versão

```bash
claude-migrate version
```

### `backup` - Backup interativo

Abre uma interface para selecionar quais sessões exportar:

```bash
claude-migrate backup
claude-migrate backup -o meu-backup.tar.gz
claude-migrate backup -v  # modo verbose
```

**Controles da TUI:**
- `↑/↓` ou `j/k` - Navegar
- `Space` ou `Enter` - Selecionar/deselecionar sessão
- `a` - Selecionar todas
- `n` - Deselecionar todas
- `/` - Buscar por path
- `b` - Criar backup das selecionadas
- `q` - Sair

### `restore` - Restaurar backup

Restaura sessões de um arquivo de backup:

```bash
# Modo interativo (pergunta os novos paths)
claude-migrate restore meu-backup.tar.gz

# Preview sem executar (dry-run)
claude-migrate restore meu-backup.tar.gz --dry-run

# Modo automático com mapeamentos
claude-migrate restore meu-backup.tar.gz \
  -m "/home/vinicius:/Users/vinicius"

# Pular prompts interativos
claude-migrate restore meu-backup.tar.gz -f

# Especificar diretório destino
claude-migrate restore meu-backup.tar.gz \
  -d ~/.claude \
  -m "/home/vinicius/projects:/Users/vinicius/dev"

# Verbose para debug
claude-migrate restore meu-backup.tar.gz -v
```

**Flags do restore:**
| Flag | Descrição |
|------|-----------|
| `-m, --mappings` | Mapeamentos de path (origem:destino) |
| `-d, --dest` | Diretório Claude destino (default: ~/.claude) |
| `--dry-run` | Preview sem executar |
| `-f, --force` | Pular prompts interativos |
| `-v, --verbose` | Logs detalhados |

### `list` - Listar sessões

```bash
# Listar sessões locais
claude-migrate list

# Listar sessões em um backup
claude-migrate list meu-backup.tar.gz
```

## Path Mapping

O formato de mapeamento é `origem:destino`:

```
/home/user/projects:/Users/user/dev
        ↑                   ↑
     ORIGEM              DESTINO
  (onde estava)       (para onde vai)
```

### Exemplos de Mapeamentos

```bash
# WSL → macOS (mesmo username)
-m "/home/vinicius:/Users/vinicius"

# WSL → macOS (username diferente)
-m "/home/vinicius:/Users/vini"

# Múltiplos mapeamentos (separados por vírgula)
-m "/home/vinicius/work:/Users/vini/trabalho,/home/vinicius/personal:/Users/vini/pessoal"

# Linux → Linux (mudou estrutura)
-m "/home/user/old-projects:/home/user/dev"
```

## Fluxo Completo: WSL → macOS

### 1. No WSL (Linux)

```bash
# Buildar
cd claude-migrate
go build -o claude-migrate .

# Iniciar backup interativo
./claude-migrate backup -o ~/claude-backup.tar.gz
```

Selecione as sessões desejadas na TUI e pressione `b`.

### 2. Transferir para o Mac

```bash
# Via SCP
scp ~/claude-backup.tar.gz usuario@mac:/Users/usuario/

# Ou via pasta compartilhada Windows
cp ~/claude-backup.tar.gz /mnt/c/Users/SeuUser/Desktop/
```

### 3. No macOS

```bash
# Buildar a ferramenta (ou copiar o binário compilado)
cd claude-migrate
go build -o claude-migrate .

# Ver o que tem no backup
./claude-migrate list ~/claude-backup.tar.gz

# Preview do restore (dry-run)
./claude-migrate restore ~/claude-backup.tar.gz --dry-run

# Restaurar interativamente
./claude-migrate restore ~/claude-backup.tar.gz
```

O restore vai perguntar o novo path para cada base diferente:

```
Path Mapping
------------
Enter the new path for each source path (or press Enter to keep same):

  /home/vinicius
  -> /Users/vinicius

Proceed with restore? [y/N]: y

Restoring 3 sessions...

  /home/vinicius/projects/meu-app
   -> /Users/vinicius/projects/meu-app
   OK: Session restored

Restore complete!
   Restored: 3 | Skipped: 0 | Failed: 0
```

### 4. Testar

```bash
cd /Users/vinicius/projects/meu-app
claude --resume
# Deve mostrar a sessão migrada!
```

## Estrutura do Backup

O arquivo `.tar.gz` contém:

```
claude-backup.tar.gz
├── manifest.json           # Metadados do backup
└── sessions/
    ├── <session-id-1>/
    │   ├── <session-id-1>.jsonl  # Conversa
    │   ├── env.json              # Variáveis de ambiente
    │   └── todos.json            # Lista de tarefas
    └── <session-id-2>/
        └── ...
```

O `manifest.json` contém:
```json
{
  "version": "1.0",
  "created_at": "2025-01-14T15:30:00Z",
  "source_host": "wsl-ubuntu",
  "source_os": "linux",
  "checksum": "sha256...",
  "total_size": 123456,
  "sessions": [
    {
      "id": "abc123-def456",
      "directory": "/home/vinicius/projects/app",
      "timestamp": "2025-01-10T14:30:00Z",
      "message_count": 42,
      "size_bytes": 45632
    }
  ]
}
```

## Troubleshooting

### Sessão não aparece após restore

1. Verifique se o path está correto:
```bash
ls ~/.claude/projects/ | grep seu-projeto
```

2. Verifique o history.jsonl:
```bash
grep "seu-projeto" ~/.claude/history.jsonl | jq .
```

3. Confirme que está no diretório certo ao rodar `claude --resume`

### Sessão já existe (skipped)

O restore detecta sessões duplicadas e pula automaticamente. Use `--dry-run` para ver o que seria restaurado.

### Erro de permissão

```bash
chmod -R 755 ~/.claude/
```

### Debug com verbose

```bash
claude-migrate restore backup.tar.gz -v
```

### Backup muito grande

Use a TUI para selecionar apenas as sessões importantes.

## Build para outras plataformas

```bash
# macOS ARM (M1/M2/M3)
GOOS=darwin GOARCH=arm64 go build -o claude-migrate-mac-arm64 .

# macOS Intel
GOOS=darwin GOARCH=amd64 go build -o claude-migrate-mac-amd64 .

# Linux
GOOS=linux GOARCH=amd64 go build -o claude-migrate-linux .

# Windows
GOOS=windows GOARCH=amd64 go build -o claude-migrate.exe .
```

## Desenvolvimento

```bash
# Rodar testes
go test -v ./...

# Build com versão
go build -ldflags "-X claude-migrate/cmd.Version=1.0.0" -o claude-migrate .
```

## Estrutura do Projeto

```
claude-migrate/
├── main.go                 # Entry point
├── cmd/
│   └── root.go             # Comandos CLI (cobra)
├── internal/
│   ├── utils/              # Utilitários (checksum, validação)
│   ├── session/            # Gerenciamento de sessões
│   ├── backup/             # Criação de backups
│   ├── restore/            # Restauração
│   └── tui/                # Interface interativa
```

## Dicas

1. **Faça backup regularmente** - A pasta `~/.claude` pode crescer bastante
2. **Use --dry-run primeiro** - Para verificar o que será restaurado
3. **Use --resume no mesmo diretório** - O Claude Code identifica sessões pelo path do projeto
4. **Mantenha usernames consistentes** - Facilita a migração sem mapeamentos
5. **Cross-compile** - Builde no WSL para macOS diretamente com GOOS/GOARCH
