use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::Result;
use owo_colors::OwoColorize;
use serde::Serialize;

use crate::engine::{BackupReport, JournalEntry, RestoreReport, SecretFinding, VerifyReport};
use crate::plan::{Action, Plan, Risk};
use crate::provider::Inventory;

#[derive(Debug, Clone, Serialize)]
pub struct DoctorEntry {
    pub provider: String,
    pub display_name: String,
    pub detected: bool,
    pub path: Option<PathBuf>,
    pub items: usize,
    pub bytes: u64,
    pub running_hint: Option<String>,
}

pub fn sanitize(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\x1b' {
            i += 1;
            if i < chars.len() {
                match chars[i] {
                    '[' => {
                        i += 1;
                        while i < chars.len() {
                            let c = chars[i];
                            i += 1;
                            if ('@'..='~').contains(&c) {
                                break;
                            }
                        }
                    }
                    ']' => {
                        i += 1;
                        while i < chars.len() {
                            let c = chars[i];
                            if c == '\x07' {
                                i += 1;
                                break;
                            }
                            if c == '\x1b' && i + 1 < chars.len() && chars[i + 1] == '\\' {
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                    }
                    _ => {
                        i += 1;
                    }
                }
            }
            continue;
        }
        if matches!(ch, '\n' | '\r' | '\t') {
            out.push(' ');
        } else if !ch.is_control() {
            out.push(ch);
        }
        i += 1;
    }
    out
}

pub fn no_color_env() -> bool {
    std::env::var_os("NO_COLOR").is_some()
        || std::env::var("TERM")
            .map(|t| t.eq_ignore_ascii_case("dumb"))
            .unwrap_or(false)
}

pub fn color_stdout() -> bool {
    !no_color_env() && std::io::stdout().is_terminal()
}

pub fn color_stderr() -> bool {
    !no_color_env() && std::io::stderr().is_terminal()
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

pub fn render_table(headers: &[&str], rows: &[Vec<String>], color: bool) -> String {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    let clean_rows: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            (0..cols)
                .map(|c| row.get(c).map(|s| sanitize(s)).unwrap_or_default())
                .collect()
        })
        .collect();
    for row in &clean_rows {
        for (c, cell) in row.iter().enumerate() {
            if cell.len() > widths[c] {
                widths[c] = cell.len();
            }
        }
    }

    let mut out = String::new();
    let mut header_cells = Vec::with_capacity(cols);
    let mut sep_cells = Vec::with_capacity(cols);
    for (c, h) in headers.iter().enumerate() {
        let padded = format!("{:<width$}", h, width = widths[c]);
        if color {
            header_cells.push(format!("{}", padded.bold()));
        } else {
            header_cells.push(padded);
        }
        sep_cells.push("-".repeat(widths[c]));
    }
    out.push_str(&header_cells.join("  "));
    out.push('\n');
    out.push_str(&sep_cells.join("  "));
    out.push('\n');

    for row in &clean_rows {
        let mut line = Vec::with_capacity(cols);
        for (c, cell) in row.iter().enumerate() {
            if c + 1 == cols {
                line.push(cell.clone());
            } else {
                line.push(format!("{:<width$}", cell, width = widths[c]));
            }
        }
        out.push_str(&line.join("  "));
        out.push('\n');
    }
    out
}

fn style_action(action: &Action, padded: &str, color: bool) -> String {
    if !color {
        return padded.to_string();
    }
    match action {
        Action::Create => format!("{}", padded.green()),
        Action::Overwrite => format!("{}", padded.yellow()),
        Action::Merge => format!("{}", padded.cyan()),
        Action::Skip(_) => format!("{}", padded.dimmed()),
        Action::Refuse(_) => format!("{}", padded.red().bold()),
    }
}

fn style_risk(risk: Risk, padded: &str, color: bool) -> String {
    if !color {
        return padded.to_string();
    }
    match risk {
        Risk::Normal => padded.to_string(),
        Risk::Sensitive => format!("{}", padded.yellow().bold()),
        Risk::ExecutesCode => format!("{}", padded.red().bold()),
    }
}

pub fn action_label(action: &Action) -> String {
    match action {
        Action::Create => "create".to_string(),
        Action::Overwrite => "overwrite".to_string(),
        Action::Merge => "merge".to_string(),
        Action::Skip(reason) => format!("skip ({})", sanitize(reason)),
        Action::Refuse(reason) => format!("REFUSE ({})", sanitize(reason)),
    }
}

pub fn risk_label(risk: Risk) -> &'static str {
    match risk {
        Risk::Normal => "normal",
        Risk::Sensitive => "SENSITIVE",
        Risk::ExecutesCode => "EXECUTES_CODE",
    }
}

pub fn format_plan(plan: &Plan, color: bool) -> String {
    let mut out = String::new();
    let headers = [
        "PROVIDER",
        "ACTION",
        "RISK",
        "SIZE",
        "REWRITES",
        "DESTINATION",
        "NOTE",
    ];
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();

    struct RawRow {
        cells: [String; 7],
        action: Action,
        risk: Risk,
    }

    let mut rows = Vec::with_capacity(plan.ops.len());
    for op in &plan.ops {
        let cells = [
            sanitize(&op.provider),
            action_label(&op.action),
            risk_label(op.risk).to_string(),
            human_size(op.size),
            op.rewrites.to_string(),
            sanitize(&op.dest.display().to_string()),
            sanitize(&op.note),
        ];
        for (i, cell) in cells.iter().enumerate() {
            if cell.len() > widths[i] {
                widths[i] = cell.len();
            }
        }
        rows.push(RawRow {
            cells,
            action: op.action.clone(),
            risk: op.risk,
        });
    }

    if !rows.is_empty() {
        let mut header_line = Vec::with_capacity(headers.len());
        let mut sep_line = Vec::with_capacity(headers.len());
        for (i, h) in headers.iter().enumerate() {
            let padded = format!("{:<width$}", h, width = widths[i]);
            if color {
                header_line.push(format!("{}", padded.bold()));
            } else {
                header_line.push(padded);
            }
            sep_line.push("-".repeat(widths[i]));
        }
        out.push_str(&header_line.join("  "));
        out.push('\n');
        out.push_str(&sep_line.join("  "));
        out.push('\n');

        for row in &rows {
            let p_prov = format!("{:<width$}", row.cells[0], width = widths[0]);
            let p_act = format!("{:<width$}", row.cells[1], width = widths[1]);
            let p_risk = format!("{:<width$}", row.cells[2], width = widths[2]);
            let p_size = format!("{:<width$}", row.cells[3], width = widths[3]);
            let p_rew = format!("{:<width$}", row.cells[4], width = widths[4]);
            let p_dest = format!("{:<width$}", row.cells[5], width = widths[5]);
            let p_note = row.cells[6].clone();
            let line = [
                p_prov,
                style_action(&row.action, &p_act, color),
                style_risk(row.risk, &p_risk, color),
                p_size,
                p_rew,
                p_dest,
                p_note,
            ];
            out.push_str(&line.join("  "));
            out.push('\n');
        }
    } else {
        out.push_str("No operations planned.\n");
    }

    for warning in &plan.warnings {
        let clean = sanitize(warning);
        if color {
            out.push_str(&format!("{}: {}\n", "warning".yellow().bold(), clean));
        } else {
            out.push_str(&format!("warning: {clean}\n"));
        }
    }

    let summary = plan.summary();
    let summary_line = format!(
        "Plan summary: create={}, overwrite={}, merge={}, skip={}, refuse={} | total={}",
        summary.create,
        summary.overwrite,
        summary.merge,
        summary.skip,
        summary.refuse,
        human_size(summary.bytes)
    );
    if color {
        out.push_str(&format!("{}\n", summary_line.bold()));
    } else {
        out.push_str(&format!("{summary_line}\n"));
    }

    let exec_part = format!("executes_code={}", summary.executes_code);
    let sens_part = format!("sensitive={}", summary.sensitive);
    let exec_styled = if color && summary.executes_code > 0 {
        format!("{}", exec_part.red().bold())
    } else {
        exec_part
    };
    let sens_styled = if color && summary.sensitive > 0 {
        format!("{}", sens_part.yellow().bold())
    } else {
        sens_part
    };
    out.push_str(&format!("Risk counts: {exec_styled}, {sens_styled}\n"));

    out
}

pub fn print_plan(plan: &Plan, to_stderr: bool) {
    if to_stderr {
        let rendered = format_plan(plan, color_stderr());
        eprint!("{rendered}");
    } else {
        let rendered = format_plan(plan, color_stdout());
        print!("{rendered}");
    }
}

pub fn format_doctor(entries: &[DoctorEntry], color: bool) -> String {
    let headers = [
        "PROVIDER", "NAME", "STATUS", "PATH", "ITEMS", "SIZE", "RUNNING",
    ];
    let rows: Vec<Vec<String>> = entries
        .iter()
        .map(|e| {
            vec![
                e.provider.clone(),
                e.display_name.clone(),
                if e.detected {
                    "detected".to_string()
                } else {
                    "missing".to_string()
                },
                e.path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "-".to_string()),
                e.items.to_string(),
                human_size(e.bytes),
                e.running_hint.clone().unwrap_or_else(|| "-".to_string()),
            ]
        })
        .collect();
    render_table(&headers, &rows, color)
}

pub fn format_inventories(inventories: &[Inventory], color: bool) -> String {
    if inventories.is_empty() {
        return "No providers found.\n".to_string();
    }
    let mut out = String::new();
    let headers = ["PROVIDER", "ID", "KIND", "FILES", "SIZE", "CWD", "LABEL"];
    let mut rows = Vec::new();
    for inv in inventories {
        for item in &inv.items {
            let kind = serde_json::to_value(item.kind)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| "item".to_string());
            rows.push(vec![
                inv.provider.as_str().to_string(),
                item.id.clone(),
                kind,
                item.files.len().to_string(),
                human_size(item.total_size()),
                item.cwd.clone().unwrap_or_else(|| "-".to_string()),
                item.label.clone(),
            ]);
        }
    }
    out.push_str(&render_table(&headers, &rows, color));
    for inv in inventories {
        for warn in &inv.warnings {
            let clean = sanitize(warn);
            if color {
                out.push_str(&format!(
                    "{} ({}): {}\n",
                    "warning".yellow().bold(),
                    inv.provider.as_str(),
                    clean
                ));
            } else {
                out.push_str(&format!("warning ({}): {clean}\n", inv.provider.as_str()));
            }
        }
    }
    out
}

pub fn format_backup_report(report: &BackupReport, color: bool) -> String {
    let status = if color {
        format!("{}", "Backup complete.".green().bold())
    } else {
        "Backup complete.".to_string()
    };
    let mut out = format!(
        "{status}\n  Output: {}\n  Encrypted: {}\n  Sessions: {}\n  Provider items: {}\n  Files: {}\n  Size: {}\n  Secret findings: {}\n",
        sanitize(&report.output.display().to_string()),
        report.encrypted,
        report.sessions,
        report.provider_items,
        report.files,
        human_size(report.bytes),
        report.secret_findings
    );
    if !report.changed_during_backup.is_empty() {
        out.push_str(&format!(
            "  Warning: {} file(s) changed during the backup; the archive holds the copy taken at that moment\n",
            report.changed_during_backup.len()
        ));
        for path in report.changed_during_backup.iter().take(10) {
            out.push_str(&format!("    {}\n", sanitize(path)));
        }
    }
    if report.secret_findings > 0 && !report.encrypted {
        out.push_str(
            "  Warning: this unencrypted archive may contain secrets; keep it private or back up again with --encrypt (see the audit command)\n",
        );
    }
    out
}

pub fn format_restore_report(report: &RestoreReport, color: bool) -> String {
    let status = if report.failed.is_empty() {
        if color {
            format!("{}", "Restore complete.".green().bold())
        } else {
            "Restore complete.".to_string()
        }
    } else if color {
        format!("{}", "Restore completed with failures.".red().bold())
    } else {
        "Restore completed with failures.".to_string()
    };
    let mut out = format!(
        "{status}\n  Written: {}\n  Merged: {}\n  Skipped: {}\n  Failed: {}\n",
        report.written,
        report.merged,
        report.skipped,
        report.failed.len()
    );
    if let Some(jid) = &report.journal_id {
        out.push_str(&format!("  Journal ID: {}\n", sanitize(jid)));
    }
    for fail in &report.failed {
        out.push_str(&format!("  Error: {}\n", sanitize(fail)));
    }
    out
}

pub fn format_verify_report(report: &VerifyReport, color: bool) -> String {
    let ok = report.ok();
    let header = if ok {
        if color {
            format!("{}", "Archive verification OK.".green().bold())
        } else {
            "Archive verification OK.".to_string()
        }
    } else if color {
        format!("{}", "Archive verification FAILED.".red().bold())
    } else {
        "Archive verification FAILED.".to_string()
    };
    let mut out = format!(
        "{header}\n  Schema: v{}\n  Encrypted: {}\n  Checked files: {}\n",
        report.schema_version, report.encrypted, report.checked
    );
    for m in &report.mismatches {
        out.push_str(&format!("  Mismatch: {}\n", sanitize(m)));
    }
    for u in &report.unexpected_entries {
        out.push_str(&format!("  Unexpected entry: {}\n", sanitize(u)));
    }
    for s in &report.unsafe_entries {
        out.push_str(&format!("  Unsafe entry: {}\n", sanitize(s)));
    }
    out
}

pub fn format_audit(findings: &[SecretFinding], color: bool) -> String {
    if findings.is_empty() {
        return "No secret findings detected.\n".to_string();
    }
    let headers = ["PROVIDER", "PATH", "DETECTOR", "COUNT"];
    let rows: Vec<Vec<String>> = findings
        .iter()
        .map(|f| {
            vec![
                f.provider.clone(),
                f.rel.clone(),
                f.detector.clone(),
                f.count.to_string(),
            ]
        })
        .collect();
    render_table(&headers, &rows, color)
}

pub fn format_journal(entries: &[JournalEntry], color: bool) -> String {
    if entries.is_empty() {
        return "No journal entries found.\n".to_string();
    }
    let headers = ["ID", "CREATED", "ARCHIVE", "OPS", "ROLLED_BACK"];
    let rows: Vec<Vec<String>> = entries
        .iter()
        .map(|e| {
            vec![
                e.id.clone(),
                e.created_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
                e.archive.clone(),
                e.ops.to_string(),
                e.rolled_back.to_string(),
            ]
        })
        .collect();
    render_table(&headers, &rows, color)
}

pub fn print_json<T: Serialize>(value: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    println!("{text}");
    Ok(())
}
