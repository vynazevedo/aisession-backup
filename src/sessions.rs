use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::convert::{Dropped, Part, Role, Transcript};
use crate::plan::{Action, Plan, PlannedOp, Risk};
use crate::provider::{Item, ItemKind, LocateEnv, ProviderId, ScanOptions};
use crate::safety::WriteMode;

pub const TRANSCRIPT_PROVIDERS: [ProviderId; 2] = [ProviderId::Claude, ProviderId::Codex];

pub struct SessionFile {
    pub provider: ProviderId,
    pub home: PathBuf,
    pub rel: String,
    pub item: Item,
}

impl SessionFile {
    pub fn path(&self) -> PathBuf {
        self.home.join(&self.rel)
    }
}

pub fn session_files(env: &LocateEnv, providers: &[ProviderId]) -> Result<Vec<SessionFile>> {
    let mut out = Vec::new();
    for prov in crate::provider::registry() {
        let id = prov.id();
        if !providers.contains(&id) || !TRANSCRIPT_PROVIDERS.contains(&id) {
            continue;
        }
        let Some(home) = prov.locate(env).filter(|home| home.exists()) else {
            continue;
        };
        let inventory = prov.scan(&home, &ScanOptions::default())?;
        for item in inventory.items {
            if item.kind != ItemKind::Session {
                continue;
            }
            let Some(rel) = item
                .files
                .iter()
                .find(|file| {
                    file.rel.ends_with(".jsonl")
                        && (id == ProviderId::Codex
                            || file.rel.ends_with(&format!("{}.jsonl", item.id)))
                })
                .map(|file| file.rel.clone())
            else {
                continue;
            };
            out.push(SessionFile {
                provider: id,
                home: home.clone(),
                rel,
                item,
            });
        }
    }
    Ok(out)
}

pub fn read_session(path: &Path) -> Result<Vec<u8>> {
    crate::archive::ensure_regular(path).with_context(|| format!("reading {}", path.display()))?;
    let size = std::fs::metadata(path)?.len();
    if size > crate::convert::MAX_SOURCE_BYTES {
        bail!(
            "session file is larger than {} bytes",
            crate::convert::MAX_SOURCE_BYTES
        );
    }
    Ok(std::fs::read(path)?)
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    pub fn add(&mut self, other: &Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }

    fn saturating_delta(&self, previous: &Usage) -> Usage {
        Usage {
            input: self.input.saturating_sub(previous.input),
            output: self.output.saturating_sub(previous.output),
            cache_read: self.cache_read.saturating_sub(previous.cache_read),
            cache_write: self.cache_write.saturating_sub(previous.cache_write),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct SessionUsage {
    pub total: Usage,
    pub by_model: BTreeMap<String, Usage>,
}

impl SessionUsage {
    fn record(&mut self, model: &str, usage: Usage) {
        self.total.add(&usage);
        self.by_model
            .entry(model.to_string())
            .or_default()
            .add(&usage);
    }
}

fn json_lines(bytes: &[u8]) -> impl Iterator<Item = Value> + '_ {
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
        .filter_map(|line| serde_json::from_slice(line).ok())
}

fn number(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn claude_usage(bytes: &[u8]) -> SessionUsage {
    let mut usage = SessionUsage::default();
    let mut seen = HashSet::new();
    for line in json_lines(bytes) {
        if line.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(message) = line.get("message") else {
            continue;
        };
        let Some(counts) = message.get("usage") else {
            continue;
        };
        if let Some(id) = message.get("id").and_then(Value::as_str) {
            if !seen.insert(id.to_string()) {
                continue;
            }
        }
        let model = message
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            .unwrap_or("unknown");
        usage.record(
            model,
            Usage {
                input: number(counts, "input_tokens"),
                output: number(counts, "output_tokens"),
                cache_read: number(counts, "cache_read_input_tokens"),
                cache_write: number(counts, "cache_creation_input_tokens"),
            },
        );
    }
    usage
}

fn codex_counts(value: &Value) -> Usage {
    let input = number(value, "input_tokens");
    let cached = number(value, "cached_input_tokens");
    Usage {
        input: input.saturating_sub(cached),
        output: number(value, "output_tokens"),
        cache_read: cached,
        cache_write: 0,
    }
}

fn codex_usage(bytes: &[u8]) -> SessionUsage {
    let mut usage = SessionUsage::default();
    let mut model = String::from("unknown");
    let mut previous = Usage::default();
    for line in json_lines(bytes) {
        let payload = line.get("payload");
        match line.get("type").and_then(Value::as_str) {
            Some("turn_context") => {
                if let Some(name) = payload
                    .and_then(|p| p.get("model"))
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                {
                    model = name.to_string();
                }
            }
            Some("event_msg") => {
                let Some(payload) = payload else { continue };
                if payload.get("type").and_then(Value::as_str) != Some("token_count") {
                    continue;
                }
                let Some(total) = payload
                    .get("info")
                    .and_then(|info| info.get("total_token_usage"))
                else {
                    continue;
                };
                let current = codex_counts(total);
                let delta = current.saturating_delta(&previous);
                previous = current;
                if delta.total() > 0 {
                    usage.record(&model, delta);
                }
            }
            _ => {}
        }
    }
    usage
}

pub fn usage(provider: ProviderId, bytes: &[u8]) -> SessionUsage {
    match provider {
        ProviderId::Claude => claude_usage(bytes),
        ProviderId::Codex => codex_usage(bytes),
        _ => SessionUsage::default(),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub provider: ProviderId,
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub path: PathBuf,
    pub size: u64,
    pub started_at: Option<DateTime<Utc>>,
    pub last_activity: Option<DateTime<Utc>>,
    pub user_turns: usize,
    pub assistant_turns: usize,
    pub tool_calls: usize,
    pub tool_errors: usize,
    pub dropped: Dropped,
    pub usage: SessionUsage,
}

pub fn inspect(provider: ProviderId, path: &Path) -> Result<(SessionInfo, Transcript)> {
    let bytes = read_session(path)?;
    let transcript = crate::convert::read(provider, &bytes)?;
    let mut info = SessionInfo {
        provider,
        id: transcript.id.clone(),
        title: transcript.title(),
        cwd: transcript.cwd.clone(),
        path: path.to_path_buf(),
        size: bytes.len() as u64,
        started_at: transcript
            .started_at
            .or_else(|| transcript.turns.iter().find_map(|turn| turn.timestamp)),
        last_activity: transcript
            .turns
            .iter()
            .rev()
            .find_map(|turn| turn.timestamp),
        user_turns: 0,
        assistant_turns: 0,
        tool_calls: 0,
        tool_errors: 0,
        dropped: transcript.dropped.clone(),
        usage: usage(provider, &bytes),
    };
    for turn in &transcript.turns {
        match turn.role {
            Role::User => info.user_turns += 1,
            Role::Assistant => info.assistant_turns += 1,
        }
        for part in &turn.parts {
            match part {
                Part::ToolCall { .. } => info.tool_calls += 1,
                Part::ToolResult { is_error: true, .. } => info.tool_errors += 1,
                _ => {}
            }
        }
    }
    Ok((info, transcript))
}

pub fn tool_name(provider: ProviderId) -> &'static str {
    match provider {
        ProviderId::Claude => "Claude Code",
        ProviderId::Codex => "Codex",
        ProviderId::Grok => "Grok",
        ProviderId::Gemini => "Gemini",
        ProviderId::Antigravity => "Antigravity",
        ProviderId::Opencode => "OpenCode",
        ProviderId::Maestri => "Maestri",
    }
}

pub fn clean_text(raw: &str) -> String {
    raw.split('\n')
        .map(|line| crate::ui::sanitize(&line.replace('\t', "    ")))
        .collect::<Vec<_>>()
        .join("\n")
}

fn stamp(value: Option<DateTime<Utc>>) -> String {
    value
        .map(|time| time.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn fence(text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for ch in text.chars() {
        if ch == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

fn fenced(text: &str) -> String {
    let marker = fence(text);
    format!("{marker}\n{text}\n{marker}\n")
}

fn inline_code(text: &str) -> String {
    let marker = if text.contains('`') { "``" } else { "`" };
    format!("{marker}{}{marker}", text.replace('\n', " "))
}

pub fn format_tokens(value: u64) -> String {
    match value {
        0..=999 => value.to_string(),
        1_000..=999_999 => format!("{:.1}K", value as f64 / 1_000.0),
        1_000_000..=999_999_999 => format!("{:.1}M", value as f64 / 1_000_000.0),
        _ => format!("{:.1}B", value as f64 / 1_000_000_000.0),
    }
}

fn usage_line(usage: &Usage) -> String {
    format!(
        "{} input, {} output, {} cache read, {} cache write",
        format_tokens(usage.input),
        format_tokens(usage.output),
        format_tokens(usage.cache_read),
        format_tokens(usage.cache_write)
    )
}

fn summary_rows(info: &SessionInfo) -> Vec<(&'static str, String)> {
    let mut rows = vec![
        ("Tool", tool_name(info.provider).to_string()),
        ("Session", clean_text(&info.id)),
        ("Directory", clean_text(&info.cwd)),
        ("Started", stamp(info.started_at)),
        ("Last activity", stamp(info.last_activity)),
        (
            "Turns",
            format!(
                "{} from the user, {} from the assistant",
                info.user_turns, info.assistant_turns
            ),
        ),
        (
            "Tool calls",
            format!("{} ({} failed)", info.tool_calls, info.tool_errors),
        ),
        ("Size", crate::ui::human_size(info.size)),
    ];
    if info.usage.total.total() > 0 {
        rows.push(("Tokens", usage_line(&info.usage.total)));
        let models: Vec<String> = info
            .usage
            .by_model
            .keys()
            .map(|model| clean_text(model))
            .collect();
        rows.push(("Models", models.join(", ")));
    }
    let dropped = &info.dropped;
    let left_out = dropped.reasoning
        + dropped.images
        + dropped.injected_context
        + dropped.sidechain
        + dropped.unreadable_lines;
    if left_out > 0 {
        rows.push((
            "Left out",
            format!(
                "{} reasoning, {} images, {} injected context, {} sidechain, {} unreadable lines",
                dropped.reasoning,
                dropped.images,
                dropped.injected_context,
                dropped.sidechain,
                dropped.unreadable_lines
            ),
        ));
    }
    rows
}

pub fn render_summary(info: &SessionInfo) -> String {
    let rows = summary_rows(info);
    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    let mut out = format!("{}\n\n", clean_text(&info.title).replace('\n', " "));
    for (label, value) in rows {
        out.push_str(&format!(
            "  {label:<width$}  {}\n",
            value.replace('\n', " ")
        ));
    }
    out
}

fn role_heading(role: Role) -> &'static str {
    match role {
        Role::User => "User",
        Role::Assistant => "Assistant",
    }
}

pub fn render_markdown(info: &SessionInfo, transcript: &Transcript) -> String {
    let mut out = format!("# {}\n\n", clean_text(&info.title).replace('\n', " "));
    for (label, value) in summary_rows(info) {
        out.push_str(&format!("- **{label}:** {}\n", value.replace('\n', " ")));
    }
    for turn in &transcript.turns {
        out.push_str(&format!(
            "\n## {} ({})\n\n",
            role_heading(turn.role),
            stamp(turn.timestamp)
        ));
        for part in &turn.parts {
            match part {
                Part::Text(text) => {
                    out.push_str(&clean_text(text));
                    out.push_str("\n\n");
                }
                Part::ToolCall { name, input } => {
                    out.push_str(&format!(
                        "**Tool call** {}\n\n",
                        inline_code(&clean_text(name))
                    ));
                    if !input.trim().is_empty() {
                        out.push_str(&fenced(&clean_text(input)));
                        out.push('\n');
                    }
                }
                Part::ToolResult { output, is_error } => {
                    let label = if *is_error {
                        "**Tool error**"
                    } else {
                        "**Tool result**"
                    };
                    out.push_str(&format!("{label}\n\n"));
                    out.push_str(&fenced(&clean_text(output)));
                    out.push('\n');
                }
            }
        }
    }
    out
}

pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

const HTML_STYLE: &str = ":root{color-scheme:light dark;--bg:#fbfaf7;--fg:#1f2328;--muted:#646b73;--line:#e3e0d8;--user:#eef3fb;--tool:#f4f2ec}@media (prefers-color-scheme:dark){:root{--bg:#17181a;--fg:#e6e6e3;--muted:#9a9ea4;--line:#2c2e31;--user:#1d2633;--tool:#202224}}body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.6 system-ui,sans-serif}main{max-width:860px;margin:0 auto;padding:32px 16px}h1{font-size:1.5rem;margin:0 0 16px}dl{display:grid;grid-template-columns:max-content 1fr;gap:4px 16px;margin:0 0 32px;color:var(--muted)}dt{font-weight:600}dd{margin:0;overflow-wrap:anywhere}article{border-top:1px solid var(--line);padding:16px 0}article.user{background:var(--user);padding:16px;border-radius:8px;border:0;margin:8px 0}header{font-weight:600;margin-bottom:8px}header time{font-weight:400;color:var(--muted);margin-left:8px}p{white-space:pre-wrap;overflow-wrap:anywhere;margin:0 0 12px}pre{background:var(--tool);padding:12px;border-radius:6px;overflow-x:auto;white-space:pre-wrap;overflow-wrap:anywhere;font-size:13px}.label{font-size:13px;color:var(--muted);margin:8px 0 4px}";

pub fn render_html(info: &SessionInfo, transcript: &Transcript) -> String {
    let title = escape_html(&clean_text(&info.title).replace('\n', " "));
    let mut out = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'\">\n<title>{title}</title>\n<style>{HTML_STYLE}</style>\n</head>\n<body>\n<main>\n<h1>{title}</h1>\n<dl>\n"
    );
    for (label, value) in summary_rows(info) {
        out.push_str(&format!(
            "<dt>{label}</dt><dd>{}</dd>\n",
            escape_html(&value.replace('\n', " "))
        ));
    }
    out.push_str("</dl>\n");
    for turn in &transcript.turns {
        let class = match turn.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        out.push_str(&format!(
            "<article class=\"{class}\">\n<header>{}<time>{}</time></header>\n",
            role_heading(turn.role),
            stamp(turn.timestamp)
        ));
        for part in &turn.parts {
            match part {
                Part::Text(text) => {
                    out.push_str(&format!("<p>{}</p>\n", escape_html(&clean_text(text))));
                }
                Part::ToolCall { name, input } => {
                    out.push_str(&format!(
                        "<div class=\"label\">Tool call {}</div>\n<pre>{}</pre>\n",
                        escape_html(&clean_text(name)),
                        escape_html(&clean_text(input))
                    ));
                }
                Part::ToolResult { output, is_error } => {
                    let label = if *is_error {
                        "Tool error"
                    } else {
                        "Tool result"
                    };
                    out.push_str(&format!(
                        "<div class=\"label\">{label}</div>\n<pre>{}</pre>\n",
                        escape_html(&clean_text(output))
                    ));
                }
            }
        }
        out.push_str("</article>\n");
    }
    out.push_str("</main>\n</body>\n</html>\n");
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsRow {
    pub key: String,
    pub sessions: usize,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub since: Option<DateTime<Utc>>,
    pub sessions: usize,
    pub sessions_with_usage: usize,
    pub unreadable: usize,
    pub total: Usage,
    pub by_tool: Vec<StatsRow>,
    pub by_model: Vec<StatsRow>,
    pub by_project: Vec<StatsRow>,
}

fn rows(map: BTreeMap<String, (BTreeSet<usize>, Usage)>) -> Vec<StatsRow> {
    let mut rows: Vec<StatsRow> = map
        .into_iter()
        .map(|(key, (sessions, usage))| StatsRow {
            key,
            sessions: sessions.len(),
            usage,
        })
        .collect();
    rows.sort_by(|a, b| {
        b.usage
            .total()
            .cmp(&a.usage.total())
            .then_with(|| b.sessions.cmp(&a.sessions))
            .then_with(|| a.key.cmp(&b.key))
    });
    rows
}

pub fn collect_stats(
    env: &LocateEnv,
    providers: &[ProviderId],
    since: Option<DateTime<Utc>>,
) -> Result<Stats> {
    let files: Vec<SessionFile> = session_files(env, providers)?
        .into_iter()
        .filter(|file| match (since, file.item.updated_at) {
            (Some(since), Some(updated)) => updated >= since,
            (Some(_), None) => false,
            (None, _) => true,
        })
        .collect();
    let usages = crate::discover::parallel_map(&files, |file| {
        read_session(&file.path())
            .ok()
            .map(|bytes| usage(file.provider, &bytes))
    });
    let mut stats = Stats {
        since,
        sessions: files.len(),
        sessions_with_usage: 0,
        unreadable: 0,
        total: Usage::default(),
        by_tool: Vec::new(),
        by_model: Vec::new(),
        by_project: Vec::new(),
    };
    let mut tools: BTreeMap<String, (BTreeSet<usize>, Usage)> = BTreeMap::new();
    let mut models: BTreeMap<String, (BTreeSet<usize>, Usage)> = BTreeMap::new();
    let mut projects: BTreeMap<String, (BTreeSet<usize>, Usage)> = BTreeMap::new();
    for (index, (file, usage)) in files.iter().zip(usages).enumerate() {
        let Some(usage) = usage else {
            stats.unreadable += 1;
            continue;
        };
        if usage.total.total() > 0 {
            stats.sessions_with_usage += 1;
        }
        stats.total.add(&usage.total);
        let tool = tools
            .entry(tool_name(file.provider).to_string())
            .or_default();
        tool.0.insert(index);
        tool.1.add(&usage.total);
        let project = projects
            .entry(file.item.cwd.clone().unwrap_or_else(|| "unknown".into()))
            .or_default();
        project.0.insert(index);
        project.1.add(&usage.total);
        for (model, counts) in &usage.by_model {
            let entry = models.entry(model.clone()).or_default();
            entry.0.insert(index);
            entry.1.add(counts);
        }
    }
    stats.by_tool = rows(tools);
    stats.by_model = rows(models);
    stats.by_project = rows(projects);
    Ok(stats)
}

fn stats_table(title: &str, rows: &[StatsRow], limit: usize, color: bool) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let body: Vec<Vec<String>> = rows
        .iter()
        .take(limit)
        .map(|row| {
            vec![
                row.key.clone(),
                row.sessions.to_string(),
                format_tokens(row.usage.input),
                format_tokens(row.usage.output),
                format_tokens(row.usage.cache_read),
                format_tokens(row.usage.cache_write),
                format_tokens(row.usage.total()),
            ]
        })
        .collect();
    let mut out = format!("\n{title}\n");
    out.push_str(&crate::ui::render_table(
        &[
            "Name",
            "Sessions",
            "Input",
            "Output",
            "Cache read",
            "Cache write",
            "Total",
        ],
        &body,
        color,
    ));
    if rows.len() > limit {
        out.push_str(&format!("  ... and {} more\n", rows.len() - limit));
    }
    out
}

pub fn format_stats(stats: &Stats, limit: usize, color: bool) -> String {
    let window = match stats.since {
        Some(since) => format!("since {}", since.format("%Y-%m-%d")),
        None => "all time".into(),
    };
    let mut out = format!(
        "{} {}, {} with token counts: {} tokens ({})\n",
        crate::ui::plural(stats.sessions, "session"),
        window,
        stats.sessions_with_usage,
        format_tokens(stats.total.total()),
        usage_line(&stats.total)
    );
    if stats.unreadable > 0 {
        out.push_str(&format!(
            "  {} could not be read\n",
            crate::ui::plural(stats.unreadable, "session")
        ));
    }
    out.push_str(&stats_table("By tool", &stats.by_tool, limit, color));
    out.push_str(&stats_table("By model", &stats.by_model, limit, color));
    out.push_str(&stats_table("By project", &stats.by_project, limit, color));
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub blocking: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResumePlan {
    pub provider: ProviderId,
    pub session_id: String,
    pub cwd: String,
    pub command: String,
    pub checks: Vec<Check>,
}

impl ResumePlan {
    pub fn blocked(&self) -> bool {
        self.checks.iter().any(|check| check.blocking && !check.ok)
    }
}

fn binary_name(provider: ProviderId) -> &'static str {
    match provider {
        ProviderId::Codex => "codex",
        _ => "claude",
    }
}

pub fn find_on_path(name: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let path = path?;
    let suffixes: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat", ""]
    } else {
        &[""]
    };
    std::env::split_paths(path).find_map(|dir| {
        suffixes.iter().find_map(|suffix| {
            let candidate = dir.join(format!("{name}{suffix}"));
            candidate.is_file().then_some(candidate)
        })
    })
}

pub fn plan_resume(
    provider: ProviderId,
    transcript: &Transcript,
    search_path: Option<&std::ffi::OsStr>,
    running: Option<String>,
) -> ResumePlan {
    let cwd = transcript.cwd.clone();
    let mut checks = Vec::new();
    let recorded = !cwd.trim().is_empty();
    checks.push(Check {
        name: "directory recorded".into(),
        ok: recorded,
        blocking: true,
        detail: if recorded {
            clean_text(&cwd)
        } else {
            "the session has no working directory".into()
        },
    });
    if recorded {
        let exists = Path::new(&cwd).is_dir();
        checks.push(Check {
            name: "directory exists".into(),
            ok: exists,
            blocking: true,
            detail: if exists {
                "found on this machine".into()
            } else {
                "missing here; use relocate to point the session at the new folder".into()
            },
        });
    }
    let binary = binary_name(provider);
    let installed = find_on_path(binary, search_path);
    checks.push(Check {
        name: format!("{binary} installed"),
        ok: installed.is_some(),
        blocking: false,
        detail: match installed {
            Some(path) => path.display().to_string(),
            None => format!("{binary} was not found on PATH"),
        },
    });
    checks.push(Check {
        name: format!("{binary} idle"),
        ok: running.is_none(),
        blocking: false,
        detail: running.unwrap_or_else(|| "no running process found".into()),
    });
    ResumePlan {
        provider,
        session_id: transcript.id.clone(),
        command: crate::convert::resume_hint(provider, &cwd, &transcript.id),
        cwd,
        checks,
    }
}

pub fn format_resume(plan: &ResumePlan) -> String {
    let mut out = format!(
        "{} session {}\n",
        tool_name(plan.provider),
        clean_text(&plan.session_id)
    );
    for check in &plan.checks {
        let mark = match (check.ok, check.blocking) {
            (true, _) => "ok  ",
            (false, true) => "FAIL",
            (false, false) => "warn",
        };
        out.push_str(&format!(
            "  [{mark}] {}: {}\n",
            check.name,
            crate::ui::sanitize(&check.detail)
        ));
    }
    if plan.blocked() {
        out.push_str("\nCannot resume until the failed checks are fixed.\n");
    } else {
        out.push_str(&format!("\n{}\n", crate::ui::sanitize(&plan.command)));
    }
    out
}

pub struct RelocateRequest {
    pub mappings: BTreeMap<String, String>,
    pub providers: Vec<ProviderId>,
    pub env: LocateEnv,
    pub allow_running: bool,
}

struct RelocateWrite {
    home: PathBuf,
    rel: String,
    bytes: Vec<u8>,
    mode: WriteMode,
}

pub struct PreparedRelocation {
    pub plan: Plan,
    pub sessions: usize,
    writes: Vec<RelocateWrite>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelocateReport {
    pub sessions: usize,
    pub written: usize,
    pub journal_id: Option<String>,
}

fn op(
    provider: ProviderId,
    dest: PathBuf,
    action: Action,
    size: u64,
    rewrites: usize,
    note: &str,
) -> PlannedOp {
    PlannedOp {
        provider: provider.as_str().into(),
        dest,
        action,
        risk: Risk::Normal,
        size,
        rewrites,
        note: note.into(),
    }
}

fn relocate_claude(
    file: &SessionFile,
    bytes: &[u8],
    mappings: &BTreeMap<String, String>,
    plan: &mut Plan,
    writes: &mut Vec<RelocateWrite>,
) -> Result<bool> {
    let Some(cwd) = file.item.cwd.as_deref() else {
        return Ok(false);
    };
    let new_cwd = crate::remap::apply_longest_mapping(cwd, mappings);
    if new_cwd == cwd {
        return Ok(false);
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        plan.ops.push(op(
            ProviderId::Claude,
            file.path(),
            Action::Skip("session is not valid UTF-8".into()),
            0,
            0,
            "",
        ));
        return Ok(true);
    };
    let (rewritten, stats) = crate::remap::rewrite_jsonl(text, mappings, false);
    let rel = format!(
        "projects/{}/{}.jsonl",
        crate::claudepath::encode(&new_cwd),
        file.item.id
    );
    let dest = crate::engine::checked_dest(&file.home, &rel)?;
    if dest.exists() {
        plan.ops.push(op(
            ProviderId::Claude,
            dest,
            Action::Skip("already relocated".into()),
            0,
            0,
            "",
        ));
        return Ok(true);
    }
    plan.ops.push(op(
        ProviderId::Claude,
        dest,
        Action::Create,
        rewritten.len() as u64,
        stats.lines_changed,
        "copy under the new project folder; the original stays where it is",
    ));
    writes.push(RelocateWrite {
        home: file.home.clone(),
        rel,
        bytes: rewritten.into_bytes(),
        mode: WriteMode::CreateNew,
    });
    Ok(true)
}

fn relocate_in_place(
    provider: &dyn crate::provider::Provider,
    home: &Path,
    rel: &str,
    bytes: &[u8],
    mappings: &BTreeMap<String, String>,
    plan: &mut Plan,
    writes: &mut Vec<RelocateWrite>,
) -> Result<bool> {
    let rewritten = provider.rewrite(rel, bytes, mappings)?;
    let Some(out) = rewritten.bytes.filter(|_| rewritten.changes > 0) else {
        return Ok(false);
    };
    let dest = crate::engine::checked_dest(home, rel)?;
    plan.ops.push(op(
        provider.id(),
        dest,
        Action::Overwrite,
        out.len() as u64,
        rewritten.changes,
        "rewritten in place",
    ));
    writes.push(RelocateWrite {
        home: home.to_path_buf(),
        rel: rel.to_string(),
        bytes: out,
        mode: WriteMode::Replace,
    });
    Ok(true)
}

pub fn prepare_relocation(request: &RelocateRequest) -> Result<PreparedRelocation> {
    if request.mappings.is_empty() {
        bail!("pass at least one OLD:NEW mapping");
    }
    for (old, new) in &request.mappings {
        if old.trim().is_empty() || new.trim().is_empty() {
            bail!("mapping paths cannot be empty");
        }
        if old.chars().chain(new.chars()).any(char::is_control) {
            bail!("mapping paths cannot contain control characters");
        }
    }
    let mut plan = Plan::default();
    let mut writes = Vec::new();
    let mut sessions = 0;
    let files = session_files(&request.env, &request.providers)?;
    for prov in crate::provider::registry() {
        let id = prov.id();
        if !request.providers.contains(&id) || !TRANSCRIPT_PROVIDERS.contains(&id) {
            continue;
        }
        let Some(home) = prov.locate(&request.env).filter(|home| home.exists()) else {
            continue;
        };
        if !request.allow_running {
            if let Some(hint) = prov.running_hint(&home) {
                plan.ops.push(op(
                    id,
                    home.clone(),
                    Action::Refuse(format!("{hint}; close it or pass --allow-running")),
                    0,
                    0,
                    "",
                ));
                continue;
            }
        }
        for file in files.iter().filter(|file| file.provider == id) {
            let bytes = read_session(&file.path())?;
            let touched = match id {
                ProviderId::Claude => {
                    relocate_claude(file, &bytes, &request.mappings, &mut plan, &mut writes)?
                }
                _ => relocate_in_place(
                    prov.as_ref(),
                    &home,
                    &file.rel,
                    &bytes,
                    &request.mappings,
                    &mut plan,
                    &mut writes,
                )?,
            };
            if touched {
                sessions += 1;
            }
        }
        if id == ProviderId::Codex {
            let config = home.join("config.toml");
            if config.is_file() {
                crate::archive::ensure_regular(&config)?;
                let bytes = std::fs::read(&config)?;
                relocate_in_place(
                    prov.as_ref(),
                    &home,
                    "config.toml",
                    &bytes,
                    &request.mappings,
                    &mut plan,
                    &mut writes,
                )?;
            }
        }
    }
    if sessions == 0 && !plan.has_refusals() {
        plan.warnings
            .push("no Claude Code or Codex session was recorded under the old path".into());
    }
    Ok(PreparedRelocation {
        plan,
        sessions,
        writes,
    })
}

pub fn apply_relocation(prepared: PreparedRelocation) -> Result<RelocateReport> {
    apply_relocation_with(prepared, crate::journal::Journal::new)
}

pub(crate) fn apply_relocation_with(
    prepared: PreparedRelocation,
    journal_for: impl FnOnce(&Path) -> Result<crate::journal::Journal>,
) -> Result<RelocateReport> {
    if prepared.plan.has_refusals() {
        bail!("relocation plan contains refused operations");
    }
    if prepared.writes.is_empty() {
        return Ok(RelocateReport {
            sessions: prepared.sessions,
            written: 0,
            journal_id: None,
        });
    }
    let mut journal = journal_for(Path::new("relocate"))?;
    let mut written = 0;
    for write in &prepared.writes {
        let dest = crate::engine::checked_dest(&write.home, &write.rel)?;
        let index = journal.before(&dest, &write.bytes)?;
        crate::safety::atomic_write(&write.home, &write.rel, &write.bytes, write.mode)?;
        journal.complete(index)?;
        written += 1;
    }
    Ok(RelocateReport {
        sessions: prepared.sessions,
        written,
        journal_id: Some(journal.id().to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_line(value: serde_json::Value) -> String {
        format!("{value}\n")
    }

    #[test]
    fn claude_usage_counts_each_message_once_per_model() {
        let usage = serde_json::json!({"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":100,"cache_creation_input_tokens":7});
        let body = [
            claude_line(serde_json::json!({"type":"assistant","message":{"id":"m1","model":"opus","usage":usage}})),
            claude_line(serde_json::json!({"type":"assistant","message":{"id":"m1","model":"opus","usage":usage}})),
            claude_line(serde_json::json!({"type":"assistant","message":{"id":"m2","model":"haiku","usage":usage}})),
            claude_line(serde_json::json!({"type":"user","message":{"usage":usage}})),
            "not json\n".to_string(),
        ]
        .concat();
        let counted = usage_of(ProviderId::Claude, &body);
        assert_eq!(counted.total.input, 20);
        assert_eq!(counted.total.cache_read, 200);
        assert_eq!(counted.total.cache_write, 14);
        assert_eq!(counted.by_model.len(), 2);
        assert_eq!(counted.by_model["opus"].output, 5);
    }

    fn usage_of(provider: ProviderId, body: &str) -> SessionUsage {
        usage(provider, body.as_bytes())
    }

    #[test]
    fn codex_usage_uses_running_totals_and_splits_cached_input() {
        let total = |input: u64, cached: u64, output: u64| serde_json::json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":input,"cached_input_tokens":cached,"output_tokens":output}}}});
        let body = [
            claude_line(serde_json::json!({"type":"turn_context","payload":{"model":"gpt-a"}})),
            claude_line(total(100, 40, 10)),
            claude_line(total(100, 40, 10)),
            claude_line(serde_json::json!({"type":"turn_context","payload":{"model":"gpt-b"}})),
            claude_line(total(300, 140, 30)),
        ]
        .concat();
        let counted = usage_of(ProviderId::Codex, &body);
        assert_eq!(counted.total.input, 160);
        assert_eq!(counted.total.cache_read, 140);
        assert_eq!(counted.total.output, 30);
        assert_eq!(counted.by_model["gpt-a"].input, 60);
        assert_eq!(counted.by_model["gpt-b"].output, 20);
    }

    #[test]
    fn html_escapes_every_markup_character() {
        assert_eq!(
            escape_html("<script>alert('x') & \"y\"</script>"),
            "&lt;script&gt;alert(&#39;x&#39;) &amp; &quot;y&quot;&lt;/script&gt;"
        );
    }

    #[test]
    fn clean_text_keeps_lines_and_drops_terminal_escapes() {
        assert_eq!(clean_text("a\x1b[31mb\nc\x07\td"), "ab\nc    d");
    }

    #[test]
    fn fences_outgrow_backticks_in_the_content() {
        assert_eq!(fence("plain"), "```");
        assert_eq!(fence("has ```` four"), "`````");
    }

    #[test]
    fn find_on_path_only_returns_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("claude")).unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert!(find_on_path("claude", Some(&path)).is_none());
        std::fs::write(dir.path().join("codex"), b"").unwrap();
        assert!(find_on_path("codex", Some(&path)).is_some());
        assert!(find_on_path("codex", None).is_none());
    }
}
