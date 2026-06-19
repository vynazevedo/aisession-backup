//! Command-line interface: argument parsing and command dispatch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use crate::backup;
use crate::discover;
use crate::model::Manifest;
use crate::restore::{self, RestoreOptions, Status};

#[derive(Parser)]
#[command(
    name = "claude-migrate",
    version,
    about = "Backup, restore and migrate Claude Code sessions across machines"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Enable verbose output.
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Interactively select and back up sessions into a .tar.gz.
    Backup {
        /// Output file path (default: claude-backup-<timestamp>.tar.gz).
        #[arg(short, long)]
        output: Option<String>,
        /// Also archive global state: shell-snapshots, file-history, plans.
        #[arg(long)]
        full: bool,
    },
    /// Restore sessions from a backup, remapping paths as needed.
    Restore {
        /// Path to the .tar.gz backup file.
        backup_file: String,
        /// Destination Claude directory.
        #[arg(short, long, default_value = "~/.claude")]
        dest: String,
        /// Path mappings, comma separated: old1:new1,old2:new2.
        #[arg(short, long)]
        mappings: Option<String>,
        /// Show what would happen without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Skip the interactive UI (non-interactive restore).
        #[arg(short, long)]
        force: bool,
        /// Also rewrite absolute paths embedded in tool calls (heuristic).
        #[arg(long)]
        rewrite_tool_paths: bool,
        /// Append entries to history.jsonl so projects show in the prompt.
        #[arg(long)]
        restore_history: bool,
        /// Do not verify archive checksums before restoring.
        #[arg(long)]
        no_verify_checksum: bool,
    },
    /// List sessions on disk, or inside a backup file.
    List {
        /// Optional backup file; omit to list local sessions.
        backup_file: Option<String>,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    crate::tui::set_verbose(cli.verbose);

    match cli.command {
        Commands::Backup { output, full } => cmd_backup(output, full),
        Commands::Restore {
            backup_file,
            dest,
            mappings,
            dry_run,
            force,
            rewrite_tool_paths,
            restore_history,
            no_verify_checksum,
        } => cmd_restore(
            &backup_file,
            &dest,
            mappings.as_deref(),
            dry_run,
            force,
            rewrite_tool_paths,
            restore_history,
            no_verify_checksum,
        ),
        Commands::List { backup_file } => cmd_list(backup_file.as_deref()),
    }
}

/// Expands a leading `~/` to the user's home directory.
pub fn expand_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

fn cmd_backup(output: Option<String>, full: bool) -> Result<()> {
    let claude_dir = expand_path("~/.claude");
    let output = output.unwrap_or_else(|| {
        format!(
            "claude-backup-{}.tar.gz",
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        )
    });
    crate::tui::run_backup(&claude_dir, Path::new(&output), full)
}

#[allow(clippy::too_many_arguments)]
fn cmd_restore(
    backup_file: &str,
    dest: &str,
    mappings: Option<&str>,
    dry_run: bool,
    force: bool,
    rewrite_tool_paths: bool,
    restore_history: bool,
    no_verify_checksum: bool,
) -> Result<()> {
    let claude_dir = expand_path(dest);
    let backup_path = PathBuf::from(backup_file);

    // Interactive path unless the user passed mappings or --force.
    if mappings.is_none() && !force {
        return crate::tui::run_restore(
            &backup_path,
            &claude_dir,
            dry_run,
            rewrite_tool_paths,
            restore_history,
            no_verify_checksum,
        );
    }

    let parsed = parse_mappings(mappings.unwrap_or(""))?;

    let loaded = restore::load_backup(&backup_path)?;
    verify_or_bail(&loaded, no_verify_checksum)?;

    let opts = RestoreOptions {
        claude_dir,
        mappings: parsed,
        dry_run,
        rewrite_tool_paths,
        restore_history,
    };

    if dry_run {
        println!("=== DRY RUN ===");
    }
    println!("Restoring {} sessions...\n", loaded.manifest.sessions.len());

    let results = restore::restore_sessions(
        &loaded.manifest.sessions,
        &loaded.data,
        &opts,
        |_, _, cwd| {
            println!("  {cwd}");
        },
    )?;

    print_results(&results, dry_run);

    if !loaded.extras.is_empty() {
        let n = restore::restore_extras(&opts.claude_dir, &loaded, dry_run);
        println!("  Extras restored: {n} files (shell-snapshots, file-history, plans)");
    }
    Ok(())
}

fn verify_or_bail(loaded: &restore::LoadedBackup, skip: bool) -> Result<()> {
    if skip {
        return Ok(());
    }
    let report = restore::verify_checksums(loaded);
    if report.skipped {
        eprintln!("Note: v1 backup has no per-artifact checksums; skipping verification.");
        return Ok(());
    }
    if !report.ok() {
        for m in &report.mismatches {
            eprintln!("  checksum error: {m}");
        }
        bail!(
            "checksum verification failed ({} of {} artifacts); use --no-verify-checksum to override",
            report.mismatches.len(),
            report.checked
        );
    }
    println!("Checksum OK ({} artifacts).", report.checked);
    Ok(())
}

fn print_results(results: &restore::Results, dry_run: bool) {
    println!();
    let mut total_rewritten = 0usize;
    for d in &results.details {
        if let Some(collision) = &d.case_collision {
            eprintln!(
                "  WARNING case collision: {} vs existing {} (case-insensitive target)",
                d.encoded, collision
            );
        }
        total_rewritten += d.lines_rewritten;
        if d.status == Status::Failed {
            eprintln!("  FAILED {}: {}", d.old_cwd, d.note);
        }
    }
    if dry_run {
        println!("=== DRY RUN SUMMARY ===");
        println!("Would restore: {} sessions", results.restored);
    } else {
        println!("Restore complete.");
        println!(
            "  Restored: {} | Skipped: {} | Failed: {}",
            results.restored, results.skipped, results.failed
        );
        println!("  JSONL lines with rewritten cwd: {total_rewritten}");
    }
}

fn cmd_list(backup_file: Option<&str>) -> Result<()> {
    match backup_file {
        Some(file) => list_backup(Path::new(file)),
        None => list_local(),
    }
}

fn list_backup(path: &Path) -> Result<()> {
    let loaded = restore::load_backup(path)?;
    print_manifest(&loaded.manifest, path);
    Ok(())
}

fn print_manifest(manifest: &Manifest, path: &Path) {
    println!(
        "Backup: {}",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    );
    print!("{}", backup::summarize(manifest));
    println!();
    for s in &manifest.sessions {
        println!(
            "  {}",
            if s.cwd.is_empty() {
                "<unknown cwd>"
            } else {
                &s.cwd
            }
        );
        println!("     id: {}", s.id);
        println!(
            "     messages: {} | size: {} | {}",
            s.message_count,
            human_size(s.size_bytes),
            s.timestamp.format("%Y-%m-%d %H:%M")
        );
        if !s.preview.is_empty() {
            println!("     first: {}", s.preview);
        }
    }
}

fn list_local() -> Result<()> {
    let claude_dir = expand_path("~/.claude");
    let sessions = discover::load_sessions(&claude_dir).context("scanning sessions")?;
    println!(
        "Found {} sessions in {}\n",
        sessions.len(),
        claude_dir.display()
    );
    for ds in &sessions {
        let s = &ds.session;
        let cwd = if s.cwd.is_empty() {
            "<unknown cwd>"
        } else {
            &s.cwd
        };
        println!("  {cwd}");
        println!("     id: {}", s.id);
        println!(
            "     messages: {} | size: {} | {} | artifacts: {}",
            s.message_count,
            human_size(s.size_bytes),
            s.timestamp.format("%Y-%m-%d %H:%M"),
            ds.companions.len() + 1
        );
        if !s.preview.is_empty() {
            println!("     first: {}", s.preview);
        }
    }
    Ok(())
}

/// Parses `old1:new1,old2:new2`, validating that both sides are absolute.
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
        let (old, new) = piece
            .split_once(':')
            .with_context(|| format!("invalid mapping '{piece}' (expected old:new)"))?;
        if !is_absolute_like(old) || !is_absolute_like(new) {
            bail!("mapping paths must be absolute: '{piece}'");
        }
        out.insert(old.to_string(), new.to_string());
    }
    Ok(out)
}

/// Absolute on unix (`/...`) or windows (`C:\...`).
fn is_absolute_like(p: &str) -> bool {
    p.starts_with('/')
        || (p.len() >= 3
            && p.as_bytes()[1] == b':'
            && (p.as_bytes()[2] == b'\\' || p.as_bytes()[2] == b'/'))
}

pub fn human_size(bytes: u64) -> String {
    const UNIT: f64 = 1024.0;
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64;
    let units = ["KB", "MB", "GB", "TB", "PB"];
    let mut idx = 0;
    size /= UNIT;
    while size >= UNIT && idx < units.len() - 1 {
        size /= UNIT;
        idx += 1;
    }
    format!("{size:.1} {}", units[idx])
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
    fn parse_mappings_accepts_windows() {
        let m = parse_mappings("/home/u:C:\\Users\\u");
        assert!(m.is_ok());
    }

    #[test]
    fn human_size_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
    }
}
