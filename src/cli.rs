use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};

use crate::backup;
use crate::discover;
use crate::engine::{
    self, BackupRequest, ClaudeSelection, Passphrase, ProviderSelection, RestoreRequest,
};
use crate::model::Manifest;
use crate::plan::Action;
use crate::provider::{self, LocateEnv, ProviderId, ScanOptions};
use crate::ui::{self, sanitize, DoctorEntry};

pub const EXIT_OK: u8 = 0;
pub const EXIT_ERROR: u8 = 1;
pub const EXIT_USAGE: u8 = 2;
pub const EXIT_CONFIRM_REQUIRED: u8 = 3;
pub const EXIT_VERIFY_FAILED: u8 = 4;
pub const EXIT_PLAN_REFUSED: u8 = 5;

#[derive(Debug)]
pub struct CliExit {
    pub code: u8,
    pub message: String,
}

impl CliExit {
    pub fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for CliExit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for CliExit {}

pub fn exit_code(err: &anyhow::Error) -> u8 {
    if let Some(exit) = err.downcast_ref::<CliExit>() {
        return exit.code;
    }
    if let Some(clap_err) = err.downcast_ref::<clap::Error>() {
        use clap::error::ErrorKind::*;
        return match clap_err.kind() {
            DisplayHelp | DisplayVersion => EXIT_OK,
            _ => EXIT_USAGE,
        };
    }
    EXIT_ERROR
}

#[derive(Parser)]
#[command(
    name = "aisession-backup",
    version,
    about = "Backup, restore and migrate AI coding sessions across machines"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    #[arg(short, long, global = true, help = "Enable verbose output")]
    pub verbose: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    #[command(about = "Inspect detected providers, paths, sizes and running hints")]
    Doctor {
        #[arg(long, help = "Output JSON to stdout")]
        json: bool,
    },
    #[command(about = "Scan providers for sessions, configs and workspaces")]
    Scan {
        #[arg(
            short,
            long,
            help = "Comma-separated providers (claude,codex,grok,gemini,agy,opencode,maestri|all)"
        )]
        provider: Option<String>,
        #[arg(long, help = "Include opaque state databases and history")]
        include_state: bool,
        #[arg(long, help = "Include login credentials")]
        include_credentials: bool,
        #[arg(long, help = "Show a flat table instead of grouping by directory")]
        flat: bool,
        #[arg(long, help = "Output JSON to stdout")]
        json: bool,
    },
    #[command(about = "Back up sessions and provider items into an archive")]
    Backup {
        #[arg(
            short,
            long,
            help = "Comma-separated providers (claude,codex,grok,gemini,agy,opencode,maestri|all)"
        )]
        provider: Option<String>,
        #[arg(
            long = "match",
            value_name = "SUBSTR",
            help = "Filter items whose cwd contains SUBSTR"
        )]
        match_path: Option<String>,
        #[arg(long, help = "Include Claude auxiliary state directories")]
        full: bool,
        #[arg(long, help = "Include global configuration files")]
        config: bool,
        #[arg(long, help = "Include provider state databases and history")]
        include_state: bool,
        #[arg(long, help = "Include login credentials (requires --encrypt)")]
        include_credentials: bool,
        #[arg(long, help = "Shorthand for --full --config")]
        all: bool,
        #[arg(long, help = "Encrypt archive with age passphrase")]
        encrypt: bool,
        #[arg(short, long, help = "Output archive file path")]
        output: Option<String>,
        #[arg(long, help = "Overwrite output archive if it already exists")]
        overwrite: bool,
        #[arg(long, help = "Print the backup plan and exit without writing")]
        dry_run: bool,
        #[arg(long, help = "Output JSON to stdout")]
        json: bool,
        #[arg(short, long, help = "Skip confirmation prompt")]
        yes: bool,
    },
    #[command(about = "Restore sessions and provider items from an archive")]
    Restore {
        #[arg(value_name = "FILE", help = "Path to the backup archive")]
        backup_file: String,
        #[arg(short, long, help = "Destination Claude directory")]
        dest: Option<String>,
        #[arg(
            long = "provider-home",
            value_name = "ID=PATH",
            help = "Destination for one provider, repeatable; with --dest, providers without it are refused"
        )]
        provider_home: Vec<String>,
        #[arg(
            short,
            long,
            help = "Comma-separated path mappings (old1:new1,old2:new2)"
        )]
        mappings: Option<String>,
        #[arg(
            short,
            long,
            help = "Only restore the specified comma-separated providers"
        )]
        provider: Option<String>,
        #[arg(long, help = "Print the restore plan and exit without writing")]
        dry_run: bool,
        #[arg(long, help = "Output JSON to stdout")]
        json: bool,
        #[arg(
            short,
            long,
            alias = "force",
            short_alias = 'f',
            help = "Skip confirmation prompt"
        )]
        yes: bool,
        #[arg(long, help = "Apply archived global configuration")]
        config: bool,
        #[arg(
            long,
            alias = "overwrite-config",
            help = "Overwrite existing files and configuration entries"
        )]
        overwrite: bool,
        #[arg(long, help = "Allow restoring config that can execute code")]
        allow_exec_config: bool,
        #[arg(long, help = "Allow restoring login credentials")]
        allow_credentials: bool,
        #[arg(long, help = "Allow restoring even if a provider process is running")]
        allow_running: bool,
        #[arg(long, help = "Rewrite embedded absolute paths inside tool calls")]
        rewrite_tool_paths: bool,
        #[arg(long, help = "Append restored sessions to history.jsonl")]
        restore_history: bool,
        #[arg(long, help = "Skip checksum and entry verification before restoring")]
        no_verify_checksum: bool,
    },
    #[command(about = "Verify archive integrity, checksums and entry safety")]
    Verify {
        #[arg(value_name = "FILE", help = "Path to the backup archive")]
        file: String,
        #[arg(long, help = "Output JSON to stdout")]
        json: bool,
    },
    #[command(about = "Scan selected files for secret tokens before backup")]
    Audit {
        #[arg(
            short,
            long,
            help = "Comma-separated providers (claude,codex,grok,gemini,agy,opencode,maestri|all)"
        )]
        provider: Option<String>,
        #[arg(
            long = "match",
            value_name = "SUBSTR",
            help = "Filter items whose cwd contains SUBSTR"
        )]
        match_path: Option<String>,
        #[arg(long, help = "Include Claude auxiliary state directories")]
        full: bool,
        #[arg(long, help = "Include global configuration files")]
        config: bool,
        #[arg(long, help = "Include provider state databases and history")]
        include_state: bool,
        #[arg(long, help = "Include login credentials")]
        include_credentials: bool,
        #[arg(long, help = "Shorthand for --full --config")]
        all: bool,
        #[arg(long, help = "Output JSON to stdout")]
        json: bool,
    },
    #[command(about = "List restore journal entries")]
    History {
        #[arg(long, help = "Output JSON to stdout")]
        json: bool,
    },
    #[command(about = "Roll back a previous restore using its journal ID")]
    Rollback {
        #[arg(value_name = "ID", help = "Journal entry identifier")]
        id: String,
        #[arg(long, help = "Print the rollback plan and exit without writing")]
        dry_run: bool,
        #[arg(short, long, help = "Skip confirmation prompt")]
        yes: bool,
        #[arg(long, help = "Output JSON to stdout")]
        json: bool,
    },
    #[command(about = "List sessions on disk or inside an archive")]
    List {
        #[arg(value_name = "FILE", help = "Optional archive file to inspect")]
        backup_file: Option<String>,
        #[arg(long, help = "List items one by one instead of grouping by directory")]
        flat: bool,
        #[arg(long, help = "Output JSON to stdout")]
        json: bool,
    },
    #[command(about = "Generate shell completion scripts")]
    Completions {
        #[arg(
            value_name = "SHELL",
            help = "Target shell (bash, zsh, fish, elvish, powershell)"
        )]
        shell: clap_complete::Shell,
    },
    #[command(about = "Print version information")]
    Version,
}

pub fn run() -> Result<()> {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            use clap::error::ErrorKind::*;
            match err.kind() {
                DisplayHelp | DisplayVersion => {
                    let _ = err.print();
                    return Ok(());
                }
                _ => {
                    let _ = err.print();
                    return Err(CliExit::new(EXIT_USAGE, "").into());
                }
            }
        }
    };
    dispatch(cli)
}

pub fn dispatch(cli: Cli) -> Result<()> {
    crate::tui::set_verbose(cli.verbose);

    let Some(command) = cli.command else {
        if io::stdin().is_terminal() && io::stdout().is_terminal() {
            let claude_dir = resolve_claude_dir(None);
            return crate::tui::run_home(&claude_dir);
        }
        let mut cmd = Cli::command();
        cmd.print_help()?;
        println!();
        return Ok(());
    };

    match command {
        Commands::Doctor { json } => cmd_doctor(json),
        Commands::Scan {
            provider,
            include_state,
            include_credentials,
            flat,
            json,
        } => cmd_scan(
            provider.as_deref(),
            ScanOptions {
                include_state,
                include_credentials,
            },
            flat,
            json,
        ),
        Commands::Backup {
            provider,
            match_path,
            full,
            config,
            include_state,
            include_credentials,
            all,
            encrypt,
            output,
            overwrite,
            dry_run,
            json,
            yes,
        } => cmd_backup(BackupArgs {
            provider,
            match_path,
            full: full || all,
            config: config || all || include_credentials,
            include_state,
            include_credentials,
            encrypt,
            output,
            overwrite,
            dry_run,
            json,
            yes,
        }),
        Commands::Restore {
            backup_file,
            dest,
            provider_home,
            mappings,
            provider,
            dry_run,
            json,
            yes,
            config,
            overwrite,
            allow_exec_config,
            allow_credentials,
            allow_running,
            rewrite_tool_paths,
            restore_history,
            no_verify_checksum,
        } => cmd_restore(RestoreArgs {
            backup_file,
            dest,
            provider_home,
            mappings,
            provider,
            dry_run,
            json,
            yes,
            apply_config: config,
            overwrite,
            allow_exec_config,
            allow_credentials,
            allow_running,
            rewrite_tool_paths,
            restore_history,
            no_verify_checksum,
        }),
        Commands::Verify { file, json } => cmd_verify(Path::new(&file), json),
        Commands::Audit {
            provider,
            match_path,
            full,
            config,
            include_state,
            include_credentials,
            all,
            json,
        } => cmd_audit(
            SelectionSpec {
                provider,
                match_path,
                full: full || all,
                config: config || all || include_credentials,
                include_state,
                include_credentials,
            },
            json,
        ),
        Commands::History { json } => cmd_history(json),
        Commands::Rollback {
            id,
            dry_run,
            yes,
            json,
        } => cmd_rollback(&id, dry_run, yes, json),
        Commands::List {
            backup_file,
            flat,
            json,
        } => cmd_list(backup_file.as_deref(), flat, json),
        Commands::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "aisession-backup", &mut io::stdout());
            Ok(())
        }
        Commands::Version => {
            println!("aisession-backup {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

struct BackupArgs {
    provider: Option<String>,
    match_path: Option<String>,
    full: bool,
    config: bool,
    include_state: bool,
    include_credentials: bool,
    encrypt: bool,
    output: Option<String>,
    overwrite: bool,
    dry_run: bool,
    json: bool,
    yes: bool,
}

struct RestoreArgs {
    backup_file: String,
    dest: Option<String>,
    provider_home: Vec<String>,
    mappings: Option<String>,
    provider: Option<String>,
    dry_run: bool,
    json: bool,
    yes: bool,
    apply_config: bool,
    overwrite: bool,
    allow_exec_config: bool,
    allow_credentials: bool,
    allow_running: bool,
    rewrite_tool_paths: bool,
    restore_history: bool,
    no_verify_checksum: bool,
}

fn user_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
}

pub fn expand_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = user_home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

pub fn resolve_claude_dir(explicit: Option<&str>) -> PathBuf {
    if let Some(dest) = explicit {
        return expand_path(dest);
    }
    if let Ok(env_dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        let trimmed = env_dir.trim();
        if !trimmed.is_empty() {
            return expand_path(trimmed);
        }
    }
    expand_path("~/.claude")
}

fn locate_env_with_claude(claude_dir: &Path) -> LocateEnv {
    let mut env = LocateEnv {
        user_home: user_home_dir(),
        overrides: BTreeMap::new(),
    };
    if claude_dir.exists() {
        env.overrides
            .insert(ProviderId::Claude, claude_dir.to_path_buf());
    }
    env
}

fn fallback_provider_home(id: ProviderId, claude_dir: &Path) -> PathBuf {
    let home = user_home_dir().unwrap_or_else(|| PathBuf::from("."));
    match id {
        ProviderId::Claude => claude_dir.to_path_buf(),
        ProviderId::Codex => std::env::var_os("CODEX_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex")),
        ProviderId::Grok => home.join(".grok"),
        ProviderId::Gemini | ProviderId::Antigravity => home.join(".gemini"),
        ProviderId::Opencode => home.join(".local/share/opencode"),
        ProviderId::Maestri => std::env::var_os("MAESTRI_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".maestri")),
    }
}

pub(crate) fn default_provider_homes(claude_dir: &Path) -> BTreeMap<ProviderId, PathBuf> {
    let env = locate_env_with_claude(claude_dir);
    let mut homes = BTreeMap::new();
    for prov in provider::registry() {
        let id = prov.id();
        if id == ProviderId::Claude {
            homes.insert(id, claude_dir.to_path_buf());
        } else {
            let path = prov
                .locate(&env)
                .unwrap_or_else(|| fallback_provider_home(id, claude_dir));
            homes.insert(id, path);
        }
    }
    homes
}

pub fn parse_provider_homes(raw: &[String]) -> Result<BTreeMap<ProviderId, PathBuf>> {
    let mut homes = BTreeMap::new();
    for entry in raw {
        let safe = sanitize(entry);
        let (id, path) = entry
            .split_once('=')
            .with_context(|| format!("invalid provider home '{safe}' (expected id=path)"))?;
        let Some(id) = ProviderId::parse(id.trim()) else {
            bail!("unknown provider in '{safe}'");
        };
        if id == ProviderId::Claude {
            bail!("use --dest to choose the Claude directory");
        }
        let path = path.trim();
        if path.chars().any(char::is_control) || has_parent_component(path) {
            bail!(
                "provider home for {} has control characters or '..'",
                id.as_str()
            );
        }
        let path = expand_path(path);
        if !path.is_absolute() {
            bail!("provider home for {} must be absolute", id.as_str());
        }
        if homes.insert(id, path).is_some() {
            bail!("provider home for {} given more than once", id.as_str());
        }
    }
    Ok(homes)
}

fn restore_provider_homes(
    dest_given: bool,
    claude_dir: &Path,
    explicit: BTreeMap<ProviderId, PathBuf>,
) -> BTreeMap<ProviderId, PathBuf> {
    let mut homes = if dest_given {
        BTreeMap::new()
    } else {
        default_provider_homes(claude_dir)
    };
    homes.extend(explicit);
    homes
}

pub fn parse_providers(raw: Option<&str>, default_claude: bool) -> Result<(bool, Vec<ProviderId>)> {
    let Some(raw) = raw else {
        if default_claude {
            return Ok((false, vec![ProviderId::Claude]));
        }
        return Ok((true, ProviderId::ALL.to_vec()));
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("provider list cannot be empty");
    }
    if trimmed.eq_ignore_ascii_case("all") {
        return Ok((true, ProviderId::ALL.to_vec()));
    }
    let mut out = Vec::new();
    for part in trimmed.split(',') {
        let token = part.trim();
        if token.is_empty() {
            continue;
        }
        let Some(id) = ProviderId::parse(token) else {
            bail!("unknown provider '{token}'");
        };
        if !out.contains(&id) {
            out.push(id);
        }
    }
    if out.is_empty() {
        bail!("no valid providers specified");
    }
    Ok((false, out))
}

fn env_passphrase() -> Option<String> {
    crate::archive::env_setting("PASSPHRASE")
        .and_then(|value| value.into_string().ok())
        .filter(|value| !value.is_empty())
}

fn resolve_encrypt_passphrase(encrypt: bool) -> Result<Passphrase> {
    if !encrypt {
        return Ok(Passphrase::None);
    }
    if let Some(val) = env_passphrase() {
        return Ok(Passphrase::Value(val));
    }
    if !io::stdin().is_terminal() {
        bail!(
            "encryption passphrase required: set AISESSION_BACKUP_PASSPHRASE in non-interactive mode"
        );
    }
    let first = rpassword::prompt_password("Enter passphrase: ")
        .context("reading encryption passphrase")?;
    if first.is_empty() {
        bail!("passphrase cannot be empty");
    }
    let second =
        rpassword::prompt_password("Confirm passphrase: ").context("confirming passphrase")?;
    if first != second {
        bail!("passphrases do not match");
    }
    Ok(Passphrase::Value(first))
}

fn is_archive_encrypted(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 32];
    let Ok(n) = file.read(&mut magic) else {
        return false;
    };
    magic[..n].starts_with(crate::crypto::MAGIC)
}

pub(crate) fn resolve_decrypt_passphrase(path: &Path) -> Result<Passphrase> {
    if let Some(val) = env_passphrase() {
        return Ok(Passphrase::Value(val));
    }
    if is_archive_encrypted(path) && io::stdin().is_terminal() {
        let pass =
            rpassword::prompt_password("Enter passphrase: ").context("reading passphrase")?;
        return Ok(Passphrase::Value(pass));
    }
    Ok(Passphrase::None)
}

fn confirm_or_abort(plan: &crate::plan::Plan, yes: bool, json_mode: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    ui::print_plan(plan, json_mode);
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(CliExit::new(
            EXIT_CONFIRM_REQUIRED,
            "confirmation required: pass --yes to proceed without a TTY",
        )
        .into());
    }
    eprint!("Proceed with these changes? [y/N]: ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let answer = line.trim().to_ascii_lowercase();
    if answer != "y" && answer != "yes" {
        return Err(CliExit::new(EXIT_CONFIRM_REQUIRED, "confirmation declined").into());
    }
    Ok(())
}

struct SelectionSpec {
    provider: Option<String>,
    match_path: Option<String>,
    full: bool,
    config: bool,
    include_state: bool,
    include_credentials: bool,
}

fn build_selection_request(
    spec: SelectionSpec,
    output: PathBuf,
    overwrite: bool,
    passphrase: Passphrase,
) -> Result<BackupRequest> {
    let claude_dir = resolve_claude_dir(None);
    let env = locate_env_with_claude(&claude_dir);
    let (is_all, ids) = parse_providers(spec.provider.as_deref(), true)?;

    let mut claude = None;
    let mut providers = Vec::new();

    for id in ids {
        if id == ProviderId::Claude {
            if !is_all || claude_dir.exists() {
                claude = Some(ClaudeSelection {
                    claude_dir: claude_dir.clone(),
                    session_ids: None,
                    match_path: spec.match_path.clone(),
                    include_extras: spec.full || spec.include_state,
                    include_config: spec.config,
                    include_credentials: spec.include_credentials,
                });
            }
        } else {
            let Some(prov) = provider::find(id) else {
                continue;
            };
            let located = prov.locate(&env);
            let home = match located {
                Some(h) => h,
                None if is_all => continue,
                None => fallback_provider_home(id, &claude_dir),
            };
            let item_ids = if let Some(substr) = &spec.match_path {
                let inv = prov.scan(
                    &home,
                    &ScanOptions {
                        include_state: spec.include_state,
                        include_credentials: spec.include_credentials,
                    },
                )?;
                let matched: Vec<String> = inv
                    .items
                    .into_iter()
                    .filter(|item| {
                        item.cwd
                            .as_ref()
                            .map(|c| c.contains(substr))
                            .unwrap_or(false)
                            || item.label.contains(substr)
                    })
                    .map(|item| item.id)
                    .collect();
                Some(matched)
            } else {
                None
            };
            providers.push(ProviderSelection {
                provider: id,
                home,
                item_ids,
                include_state: spec.include_state,
                include_credentials: spec.include_credentials,
            });
        }
    }

    if claude.is_none() && providers.is_empty() {
        claude = Some(ClaudeSelection {
            claude_dir,
            session_ids: None,
            match_path: spec.match_path,
            include_extras: spec.full || spec.include_state,
            include_config: spec.config,
            include_credentials: spec.include_credentials,
        });
    }

    Ok(BackupRequest {
        output,
        overwrite,
        claude,
        providers,
        passphrase,
    })
}

fn cmd_doctor(json: bool) -> Result<()> {
    let claude_dir = resolve_claude_dir(None);
    let env = locate_env_with_claude(&claude_dir);
    let mut entries = Vec::new();

    for prov in provider::registry() {
        let home = prov.locate(&env);
        let detected = home.as_ref().map(|h| h.exists()).unwrap_or(false);
        let mut items = 0usize;
        let mut bytes = 0u64;
        let mut running_hint = None;
        if let Some(h) = &home {
            if h.exists() {
                if let Ok(inv) = prov.scan(h, &ScanOptions::default()) {
                    items = inv.items.len();
                    bytes = inv.items.iter().map(provider::Item::total_size).sum();
                }
                running_hint = prov.running_hint(h).map(|s| sanitize(&s));
            }
        }
        entries.push(DoctorEntry {
            provider: prov.id().as_str().to_string(),
            display_name: prov.display_name().to_string(),
            detected,
            path: home,
            items,
            bytes,
            running_hint,
        });
    }

    if json {
        ui::print_json(&entries)?;
    } else {
        print!("{}", ui::format_doctor(&entries, ui::color_stdout()));
    }
    Ok(())
}

fn cmd_scan(provider_arg: Option<&str>, opts: ScanOptions, flat: bool, json: bool) -> Result<()> {
    let claude_dir = resolve_claude_dir(None);
    let env = locate_env_with_claude(&claude_dir);
    let (is_all, ids) = parse_providers(provider_arg, false)?;
    let mut inventories = Vec::new();

    for id in ids {
        let Some(prov) = provider::find(id) else {
            continue;
        };
        let home = match prov.locate(&env) {
            Some(h) => h,
            None if is_all => continue,
            None => fallback_provider_home(id, &claude_dir),
        };
        if !home.exists() && is_all {
            continue;
        }
        let inv = prov.scan(&home, &opts)?;
        inventories.push(inv);
    }

    if json {
        ui::print_json(&inventories)?;
    } else if flat {
        print!(
            "{}",
            ui::format_inventories(&inventories, ui::color_stdout())
        );
    } else {
        print!(
            "{}",
            ui::format_inventories_grouped(&inventories, ui::color_stdout())
        );
    }
    Ok(())
}

fn cmd_backup(args: BackupArgs) -> Result<()> {
    let ext = if args.encrypt { "tar.gz.age" } else { "tar.gz" };
    let output_str = args.output.unwrap_or_else(|| {
        format!(
            "claude-backup-{}.{ext}",
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        )
    });
    let output = expand_path(&output_str);
    let passphrase = resolve_encrypt_passphrase(args.encrypt)?;

    let request = build_selection_request(
        SelectionSpec {
            provider: args.provider,
            match_path: args.match_path,
            full: args.full,
            config: args.config,
            include_state: args.include_state,
            include_credentials: args.include_credentials,
        },
        output,
        args.overwrite,
        passphrase,
    )?;

    let plan = engine::plan_backup(&request)?;
    if args.dry_run {
        if args.json {
            ui::print_json(&plan)?;
        } else {
            ui::print_plan(&plan, false);
        }
        return Ok(());
    }

    if plan.has_refusals() {
        if args.json {
            ui::print_json(&plan)?;
        } else {
            ui::print_plan(&plan, false);
        }
        return Err(
            CliExit::new(EXIT_PLAN_REFUSED, "backup plan contains refused operations").into(),
        );
    }

    confirm_or_abort(&plan, args.yes, args.json)?;

    let report = engine::run_backup(&request)?;
    if args.json {
        ui::print_json(&report)?;
    } else {
        print!("{}", ui::format_backup_report(&report, ui::color_stdout()));
    }
    Ok(())
}

fn cmd_restore(args: RestoreArgs) -> Result<()> {
    let claude_dir = resolve_claude_dir(args.dest.as_deref());
    let archive = expand_path(&args.backup_file);
    let mappings = parse_mappings(args.mappings.as_deref().unwrap_or(""))?;
    let only_providers = match args.provider.as_deref() {
        Some(raw) => {
            let (is_all, ids) = parse_providers(Some(raw), false)?;
            if is_all {
                None
            } else {
                Some(ids)
            }
        }
        None => None,
    };
    let passphrase = resolve_decrypt_passphrase(&archive)?;
    let provider_homes = restore_provider_homes(
        args.dest.is_some(),
        &claude_dir,
        parse_provider_homes(&args.provider_home)?,
    );

    let request = RestoreRequest {
        archive,
        passphrase,
        claude_dir,
        provider_homes,
        only_providers,
        only_sessions: None,
        only_items: None,
        mappings,
        rewrite_tool_paths: args.rewrite_tool_paths,
        restore_history: args.restore_history,
        apply_config: args.apply_config,
        overwrite: args.overwrite,
        allow_exec_config: args.allow_exec_config,
        allow_credentials: args.allow_credentials,
        allow_running: args.allow_running,
        skip_verify: args.no_verify_checksum,
    };

    let prepared = engine::prepare_restore(request)?;
    if args.dry_run {
        if args.json {
            ui::print_json(&prepared.plan)?;
        } else {
            ui::print_plan(&prepared.plan, false);
        }
        return Ok(());
    }

    let verify_failed = !args.no_verify_checksum
        && prepared.plan.ops.iter().any(|op| {
            op.provider == "archive"
                && matches!(&op.action, Action::Refuse(reason) if reason.contains("verification failed"))
        });
    if verify_failed {
        if args.json {
            ui::print_json(&prepared.plan)?;
        } else {
            ui::print_plan(&prepared.plan, true);
        }
        return Err(CliExit::new(EXIT_VERIFY_FAILED, "archive verification failed").into());
    }

    if prepared.plan.has_refusals() {
        if args.json {
            ui::print_json(&prepared.plan)?;
        } else {
            ui::print_plan(&prepared.plan, false);
        }
        return Err(CliExit::new(
            EXIT_PLAN_REFUSED,
            "restore plan contains refused operations",
        )
        .into());
    }

    confirm_or_abort(&prepared.plan, args.yes, args.json)?;

    let report = engine::apply_restore(prepared)?;
    if args.json {
        ui::print_json(&report)?;
    } else {
        print!("{}", ui::format_restore_report(&report, ui::color_stdout()));
    }
    if !report.failed.is_empty() {
        bail!(
            "restore finished with {} failed operation(s)",
            report.failed.len()
        );
    }
    Ok(())
}

fn cmd_verify(path: &Path, json: bool) -> Result<()> {
    if !path.is_file() {
        bail!(
            "archive not found: {}",
            sanitize(&path.display().to_string())
        );
    }
    let passphrase = resolve_decrypt_passphrase(path)?;
    let report = match engine::verify_archive(path, &passphrase) {
        Ok(r) => r,
        Err(err) => {
            return Err(
                CliExit::new(EXIT_VERIFY_FAILED, format!("verification failed: {err:#}")).into(),
            );
        }
    };
    if json {
        ui::print_json(&report)?;
    } else {
        print!("{}", ui::format_verify_report(&report, ui::color_stdout()));
    }
    if !report.ok() {
        return Err(CliExit::new(EXIT_VERIFY_FAILED, "archive verification failed").into());
    }
    Ok(())
}

fn cmd_audit(spec: SelectionSpec, json: bool) -> Result<()> {
    let request = build_selection_request(
        spec,
        PathBuf::from("audit.tar.gz"),
        false,
        Passphrase::Value("audit-placeholder".to_string()),
    )?;
    let findings = engine::audit_selection(&request)?;
    if json {
        ui::print_json(&findings)?;
    } else {
        print!("{}", ui::format_audit(&findings, ui::color_stdout()));
    }
    Ok(())
}

fn cmd_history(json: bool) -> Result<()> {
    let entries = engine::list_journal()?;
    if json {
        ui::print_json(&entries)?;
    } else {
        print!("{}", ui::format_journal(&entries, ui::color_stdout()));
    }
    Ok(())
}

fn cmd_rollback(id: &str, dry_run: bool, yes: bool, json: bool) -> Result<()> {
    let plan = engine::plan_rollback(id)?;
    if dry_run {
        if json {
            ui::print_json(&plan)?;
        } else {
            ui::print_plan(&plan, false);
        }
        return Ok(());
    }
    if plan.has_refusals() {
        if json {
            ui::print_json(&plan)?;
        } else {
            ui::print_plan(&plan, false);
        }
        return Err(CliExit::new(
            EXIT_PLAN_REFUSED,
            "rollback plan contains refused operations",
        )
        .into());
    }

    confirm_or_abort(&plan, yes, json)?;

    let report = engine::rollback(id)?;
    if json {
        ui::print_json(&report)?;
    } else {
        print!("{}", ui::format_restore_report(&report, ui::color_stdout()));
    }
    if !report.failed.is_empty() {
        bail!(
            "rollback finished with {} failed operation(s)",
            report.failed.len()
        );
    }
    Ok(())
}

fn cmd_list(backup_file: Option<&str>, flat: bool, json: bool) -> Result<()> {
    match backup_file {
        Some(file) => list_backup(Path::new(file), flat, json),
        None => list_local(flat, json),
    }
}

fn session_entry(session: &crate::model::Session) -> crate::grouping::Entry {
    crate::grouping::Entry {
        id: session.id.clone(),
        label: session.preview.clone(),
        cwd: Some(session.cwd.clone()),
        provider: ProviderId::Claude.as_str().to_string(),
        detail: format!("{} msgs", session.message_count),
        size: session.size_bytes,
        updated_at: Some(session.timestamp),
    }
}

pub(crate) fn manifest_entries(manifest: &Manifest) -> Vec<crate::grouping::Entry> {
    let mut entries: Vec<_> = manifest.sessions.iter().map(session_entry).collect();
    for provider in &manifest.providers {
        for item in &provider.items {
            entries.push(crate::grouping::Entry {
                id: item.id.clone(),
                label: item.label.clone(),
                cwd: item.cwd.clone(),
                provider: provider.id.as_str().to_string(),
                detail: format!("{} file(s)", item.files.len()),
                size: item.total_size(),
                updated_at: item.updated_at,
            });
        }
    }
    entries
}

fn list_backup(path: &Path, flat: bool, json: bool) -> Result<()> {
    let passphrase = resolve_decrypt_passphrase(path)?;
    let manifest = crate::engine::read_manifest(path, &passphrase)?;
    if json {
        ui::print_json(&manifest)?;
    } else {
        print_manifest(&manifest, path, flat);
    }
    Ok(())
}

fn print_manifest(manifest: &Manifest, path: &Path, flat: bool) {
    let fname = path
        .file_name()
        .map(|n| sanitize(&n.to_string_lossy()))
        .unwrap_or_default();
    println!("Backup: {fname}");
    print!("{}", sanitize_multiline(&backup::summarize(manifest)));
    println!();
    if !flat {
        print!(
            "{}",
            crate::grouping::format_groups(&manifest_entries(manifest), ui::color_stdout())
        );
        return;
    }
    for s in &manifest.sessions {
        let cwd = if s.cwd.is_empty() {
            "<unknown cwd>".to_string()
        } else {
            sanitize(&s.cwd)
        };
        println!("  {cwd}");
        println!("     id: {}", sanitize(&s.id));
        println!(
            "     messages: {} | size: {} | {}",
            s.message_count,
            human_size(s.size_bytes),
            s.timestamp.format("%Y-%m-%d %H:%M")
        );
        if !s.preview.is_empty() {
            println!("     first: {}", sanitize(&s.preview));
        }
    }
    for p in &manifest.providers {
        println!(
            "  Provider: {} ({} items, source: {})",
            p.id.as_str(),
            p.items.len(),
            sanitize(&p.source_home)
        );
        for item in &p.items {
            println!(
                "     - {} ({}): {}",
                sanitize(&item.id),
                human_size(item.total_size()),
                sanitize(&item.label)
            );
        }
    }
}

fn sanitize_multiline(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        out.push_str(&sanitize(line));
        out.push('\n');
    }
    out
}

fn list_local(flat: bool, json: bool) -> Result<()> {
    let claude_dir = resolve_claude_dir(None);
    let sessions = discover::load_sessions(&claude_dir).context("scanning sessions")?;
    if json {
        let only_sessions: Vec<_> = sessions.into_iter().map(|d| d.session).collect();
        return ui::print_json(&only_sessions);
    }
    println!(
        "Found {} sessions in {}\n",
        sessions.len(),
        sanitize(&claude_dir.display().to_string())
    );
    if !flat {
        let entries: Vec<_> = sessions
            .iter()
            .map(|found| session_entry(&found.session))
            .collect();
        print!(
            "{}",
            crate::grouping::format_groups(&entries, ui::color_stdout())
        );
        return Ok(());
    }
    for ds in &sessions {
        let s = &ds.session;
        let cwd = if s.cwd.is_empty() {
            "<unknown cwd>".to_string()
        } else {
            sanitize(&s.cwd)
        };
        println!("  {cwd}");
        println!("     id: {}", sanitize(&s.id));
        println!(
            "     messages: {} | size: {} | {} | artifacts: {}",
            s.message_count,
            human_size(s.size_bytes),
            s.timestamp.format("%Y-%m-%d %H:%M"),
            ds.companions.len() + 1
        );
        if !s.preview.is_empty() {
            println!("     first: {}", sanitize(&s.preview));
        }
    }
    Ok(())
}

fn split_mapping(piece: &str) -> Option<(&str, &str)> {
    let bytes = piece.as_bytes();
    let start = if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        3
    } else {
        0
    };
    let rel_colon = piece.get(start..)?.find(':')?;
    let colon = start + rel_colon;
    Some((&piece[..colon], &piece[colon + 1..]))
}

pub fn parse_mappings(raw: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    if raw.trim().is_empty() {
        return Ok(out);
    }
    for piece in raw.split(',') {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        if piece.chars().any(char::is_control) {
            bail!("mapping contains control characters");
        }
        let (old, new) = split_mapping(piece)
            .with_context(|| format!("invalid mapping '{piece}' (expected old:new)"))?;
        if !is_absolute_like(old) {
            bail!("mapping paths must be absolute: '{piece}'");
        }
        validate_mapping_target(new).map_err(|reason| anyhow::anyhow!("{reason}: '{piece}'"))?;
        out.insert(old.to_string(), new.to_string());
    }
    Ok(out)
}

fn has_parent_component(path: &str) -> bool {
    path.split(['/', '\\']).any(|part| part == "..")
}

pub(crate) fn validate_mapping_target(target: &str) -> std::result::Result<(), &'static str> {
    if target.chars().any(char::is_control) {
        return Err("mapping destination contains control characters");
    }
    if !is_absolute_like(target) {
        return Err("mapping paths must be absolute");
    }
    if has_parent_component(target) {
        return Err("mapping destination cannot contain '..'");
    }
    Ok(())
}

fn is_absolute_like(p: &str) -> bool {
    p.starts_with('/')
        || (p.len() >= 3
            && p.as_bytes()[0].is_ascii_alphabetic()
            && p.as_bytes()[1] == b':'
            && (p.as_bytes()[2] == b'\\' || p.as_bytes()[2] == b'/'))
}

pub fn human_size(bytes: u64) -> String {
    ui::human_size(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mappings_ok() {
        let m = parse_mappings("/a:/b,/c/d:/e/f").unwrap();
        assert_eq!(m.get("/a"), Some(&"/b".to_string()));
        assert_eq!(m.get("/c/d"), Some(&"/e/f".to_string()));
    }

    #[test]
    fn parse_mappings_rejects_relative() {
        assert!(parse_mappings("a:/b").is_err());
        assert!(parse_mappings("/a:b").is_err());
    }

    #[test]
    fn parse_mappings_accepts_windows_source_and_target() {
        let m = parse_mappings("/home/u:C:\\Users\\u").unwrap();
        assert_eq!(m.get("/home/u"), Some(&"C:\\Users\\u".to_string()));

        let m2 = parse_mappings("C:\\a:/b,D:/work:E:\\work").unwrap();
        assert_eq!(m2.get("C:\\a"), Some(&"/b".to_string()));
        assert_eq!(m2.get("D:/work"), Some(&"E:\\work".to_string()));
    }

    #[test]
    fn human_size_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
    }
}
