use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::plan::{Action, Plan, PlannedOp, Risk};
use crate::provider::ProviderId;
use crate::safety::WriteMode;

pub const TOOL_TEXT_LIMIT: usize = 2000;
pub const MAX_SOURCE_BYTES: u64 = 512 * 1024 * 1024;
const CLAUDE_FORMAT_VERSION: &str = "2.1.0";
const CODEX_INDEX: &str = "session_index.jsonl";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    Text(String),
    ToolCall { name: String, input: String },
    ToolResult { output: String, is_error: bool },
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub role: Role,
    pub timestamp: Option<DateTime<Utc>>,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Dropped {
    pub reasoning: usize,
    pub images: usize,
    pub injected_context: usize,
    pub sidechain: usize,
    pub unreadable_lines: usize,
}

#[derive(Debug, Clone)]
pub struct Transcript {
    pub provider: ProviderId,
    pub id: String,
    pub cwd: String,
    pub started_at: Option<DateTime<Utc>>,
    pub turns: Vec<Turn>,
    pub dropped: Dropped,
}

impl Transcript {
    fn new(provider: ProviderId) -> Self {
        Self {
            provider,
            id: String::new(),
            cwd: String::new(),
            started_at: None,
            turns: Vec::new(),
            dropped: Dropped::default(),
        }
    }

    fn push(&mut self, role: Role, timestamp: Option<DateTime<Utc>>, part: Part) {
        let role = if matches!(part, Part::ToolResult { .. }) {
            Role::Assistant
        } else {
            role
        };
        match self.turns.last_mut() {
            Some(last) if last.role == role => last.parts.push(part),
            _ => self.turns.push(Turn {
                role,
                timestamp,
                parts: vec![part],
            }),
        }
    }

    fn push_text(&mut self, role: Role, timestamp: Option<DateTime<Utc>>, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        if role == Role::User && is_injected(text) {
            self.dropped.injected_context += 1;
            return;
        }
        self.push(role, timestamp, Part::Text(text.to_string()));
    }

    pub fn title(&self) -> String {
        let first = self
            .turns
            .iter()
            .find(|turn| turn.role == Role::User)
            .and_then(|turn| {
                turn.parts.iter().find_map(|part| match part {
                    Part::Text(text) => text.lines().find(|line| !line.trim().is_empty()),
                    _ => None,
                })
            });
        let title: String = first
            .unwrap_or("Imported session")
            .trim()
            .chars()
            .take(60)
            .collect();
        title
    }

    fn finish(self) -> Result<Self> {
        if self.turns.is_empty() {
            bail!("the session has no conversation to convert; it only holds automated or tool-generated messages");
        }
        if self.id.trim().is_empty() {
            bail!("the session has no identifier");
        }
        Ok(self)
    }
}

fn parse_time(value: &Value) -> Option<DateTime<Utc>> {
    value.as_str()?.parse().ok()
}

fn is_injected(text: &str) -> bool {
    let text = text.trim_start();
    if text.starts_with("# AGENTS.md instructions") {
        return true;
    }
    let Some(rest) = text.strip_prefix('<') else {
        return false;
    };
    let Some(end) = rest.find('>') else {
        return false;
    };
    let name = &rest[..end];
    !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ' '))
        && text.contains(&format!("</{name}>"))
}

fn lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(|&b| b == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub fn read_claude(bytes: &[u8]) -> Result<Transcript> {
    let mut t = Transcript::new(ProviderId::Claude);
    for line in lines(bytes) {
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            t.dropped.unreadable_lines += 1;
            continue;
        };
        if t.id.is_empty() {
            if let Some(id) = v.get("sessionId").and_then(Value::as_str) {
                t.id = id.to_string();
            }
        }
        let kind = v.get("type").and_then(Value::as_str).unwrap_or_default();
        let role = match kind {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => continue,
        };
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            t.dropped.sidechain += 1;
            continue;
        }
        if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let message = &v["message"];
        if message.get("model").and_then(Value::as_str) == Some("<synthetic>") {
            continue;
        }
        if t.cwd.is_empty() {
            if let Some(cwd) = v.get("cwd").and_then(Value::as_str) {
                t.cwd = cwd.to_string();
            }
        }
        let ts = parse_time(&v["timestamp"]);
        t.started_at = t.started_at.or(ts);
        match &message["content"] {
            Value::String(text) => t.push_text(role, ts, text),
            Value::Array(blocks) => {
                for block in blocks {
                    match block
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                    {
                        "text" => {
                            let text = block
                                .get("text")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            t.push_text(role, ts, text);
                        }
                        "thinking" | "redacted_thinking" => t.dropped.reasoning += 1,
                        "tool_use" => t.push(
                            role,
                            ts,
                            Part::ToolCall {
                                name: block["name"].as_str().unwrap_or("tool").to_string(),
                                input: block["input"].to_string(),
                            },
                        ),
                        "tool_result" => t.push(
                            role,
                            ts,
                            Part::ToolResult {
                                output: value_text(&block["content"]),
                                is_error: block["is_error"].as_bool().unwrap_or(false),
                            },
                        ),
                        "image" => t.dropped.images += 1,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    t.finish()
}

pub fn read_codex(bytes: &[u8]) -> Result<Transcript> {
    let mut t = Transcript::new(ProviderId::Codex);
    for line in lines(bytes) {
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            t.dropped.unreadable_lines += 1;
            continue;
        };
        let ts = parse_time(&v["timestamp"]);
        let payload = &v["payload"];
        match v.get("type").and_then(Value::as_str).unwrap_or_default() {
            "session_meta" => {
                if let Some(id) = payload.get("id").and_then(Value::as_str) {
                    t.id = id.to_string();
                }
                if let Some(cwd) = payload.get("cwd").and_then(Value::as_str) {
                    t.cwd = cwd.to_string();
                }
                t.started_at = parse_time(&payload["timestamp"]).or(ts);
            }
            "response_item" => {
                match payload
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "message" => {
                        let role = match payload["role"].as_str() {
                            Some("user") => Role::User,
                            Some("assistant") => Role::Assistant,
                            _ => continue,
                        };
                        for item in payload["content"].as_array().into_iter().flatten() {
                            match item.get("type").and_then(Value::as_str).unwrap_or_default() {
                                "input_text" | "output_text" => {
                                    let text = item
                                        .get("text")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default();
                                    t.push_text(role, ts, text);
                                }
                                "input_image" => t.dropped.images += 1,
                                _ => {}
                            }
                        }
                    }
                    "function_call" => t.push(
                        Role::Assistant,
                        ts,
                        Part::ToolCall {
                            name: payload["name"].as_str().unwrap_or("tool").to_string(),
                            input: value_text(&payload["arguments"]),
                        },
                    ),
                    "custom_tool_call" => t.push(
                        Role::Assistant,
                        ts,
                        Part::ToolCall {
                            name: payload["name"].as_str().unwrap_or("tool").to_string(),
                            input: value_text(&payload["input"]),
                        },
                    ),
                    "function_call_output" | "custom_tool_call_output" => t.push(
                        Role::Assistant,
                        ts,
                        Part::ToolResult {
                            output: value_text(&payload["output"]),
                            is_error: false,
                        },
                    ),
                    "reasoning" => t.dropped.reasoning += 1,
                    _ => {}
                }
            }
            _ => {}
        }
    }
    t.finish()
}

pub fn detect(bytes: &[u8]) -> Option<ProviderId> {
    for line in lines(bytes).take(200) {
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("session_meta" | "response_item" | "turn_context") => {
                return Some(ProviderId::Codex)
            }
            Some("user" | "assistant") if v.get("sessionId").is_some() => {
                return Some(ProviderId::Claude)
            }
            _ => {}
        }
    }
    None
}

pub fn read(provider: ProviderId, bytes: &[u8]) -> Result<Transcript> {
    match provider {
        ProviderId::Claude => read_claude(bytes),
        ProviderId::Codex => read_codex(bytes),
        other => bail!("converting from {} is not supported yet", other.as_str()),
    }
}

fn fence(text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

fn clip(text: &str, limit: usize) -> String {
    let total = text.chars().count();
    if total <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}\n[... {} more characters omitted]", total - limit)
}

fn render_part(part: &Part) -> String {
    match part {
        Part::Text(text) => text.clone(),
        Part::ToolCall { name, input } => {
            let body = clip(input, TOOL_TEXT_LIMIT);
            let f = fence(&body);
            format!("Tool call `{name}`:\n{f}\n{body}\n{f}")
        }
        Part::ToolResult { output, is_error } => {
            let body = clip(output, TOOL_TEXT_LIMIT);
            let f = fence(&body);
            let label = if *is_error {
                "Tool error"
            } else {
                "Tool result"
            };
            format!("{label}:\n{f}\n{body}\n{f}")
        }
    }
}

fn turn_text(turn: &Turn) -> String {
    turn.parts
        .iter()
        .map(render_part)
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn derived_uuid(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex = hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn provider_name(provider: ProviderId) -> &'static str {
    match provider {
        ProviderId::Claude => "Claude Code",
        ProviderId::Codex => "Codex",
        other => other.as_str(),
    }
}

fn preamble(t: &Transcript) -> String {
    format!(
        "[Imported by aisession-backup from {} session {}. Tool calls and results appear as text, and the original reasoning was not carried over.]",
        provider_name(t.provider),
        t.id
    )
}

fn messages(t: &Transcript) -> Vec<(Role, String, Option<DateTime<Utc>>)> {
    let mut out: Vec<(Role, String, Option<DateTime<Utc>>)> = t
        .turns
        .iter()
        .map(|turn| (turn.role, turn_text(turn), turn.timestamp))
        .collect();
    match out.first_mut() {
        Some((Role::User, text, _)) => *text = format!("{}\n\n{text}", preamble(t)),
        _ => out.insert(0, (Role::User, preamble(t), t.started_at)),
    }
    out
}

fn stamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn push_json(out: &mut Vec<u8>, value: &Value) {
    out.extend_from_slice(value.to_string().as_bytes());
    out.push(b'\n');
}

pub struct Rendered {
    pub session_id: String,
    pub rel: String,
    pub bytes: Vec<u8>,
    pub index_line: Option<Vec<u8>>,
}

pub fn write_claude(t: &Transcript, cwd: &str, now: DateTime<Utc>) -> Rendered {
    let sid = derived_uuid(&format!("claude:{}:{}", t.provider.as_str(), t.id));
    let mut out = Vec::new();
    let mut parent: Option<String> = None;
    for (index, (role, text, ts)) in messages(t).into_iter().enumerate() {
        let uuid = derived_uuid(&format!("{sid}:{index}"));
        let message = match role {
            Role::User => json!({"role": "user", "content": text}),
            Role::Assistant => json!({
                "id": format!("msg_imported_{index}"),
                "type": "message",
                "role": "assistant",
                "model": format!("imported-from-{}", t.provider.as_str()),
                "content": [{"type": "text", "text": text}],
                "stop_reason": "end_turn",
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0},
            }),
        };
        let kind = match role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        push_json(
            &mut out,
            &json!({
                "parentUuid": parent,
                "isSidechain": false,
                "userType": "external",
                "cwd": cwd,
                "sessionId": sid,
                "version": CLAUDE_FORMAT_VERSION,
                "gitBranch": "",
                "type": kind,
                "message": message,
                "uuid": uuid,
                "timestamp": stamp(ts.unwrap_or(now)),
            }),
        );
        parent = Some(uuid);
    }
    push_json(
        &mut out,
        &json!({"type": "ai-title", "aiTitle": t.title(), "sessionId": sid}),
    );
    Rendered {
        rel: format!("projects/{}/{sid}.jsonl", crate::claudepath::encode(cwd)),
        session_id: sid,
        bytes: out,
        index_line: None,
    }
}

pub fn write_codex(t: &Transcript, cwd: &str, now: DateTime<Utc>) -> Rendered {
    let sid = derived_uuid(&format!("codex:{}:{}", t.provider.as_str(), t.id));
    let mut out = Vec::new();
    let line = |out: &mut Vec<u8>, ts: &str, kind: &str, payload: Value| {
        push_json(
            out,
            &json!({"timestamp": ts, "type": kind, "payload": payload}),
        );
    };
    let start = stamp(now);
    line(
        &mut out,
        &start,
        "session_meta",
        json!({
            "id": sid,
            "session_id": sid,
            "timestamp": start,
            "cwd": cwd,
            "originator": "codex_cli_rs",
            "cli_version": "0.0.0",
            "source": "cli",
            "model_provider": "openai",
        }),
    );
    for (role, text, ts) in messages(t) {
        let ts = stamp(ts.unwrap_or(now));
        let (role_name, content_type, event) = match role {
            Role::User => ("user", "input_text", "user_message"),
            Role::Assistant => ("assistant", "output_text", "agent_message"),
        };
        line(
            &mut out,
            &ts,
            "response_item",
            json!({
                "type": "message",
                "role": role_name,
                "content": [{"type": content_type, "text": text}],
            }),
        );
        line(
            &mut out,
            &ts,
            "event_msg",
            json!({"type": event, "message": text}),
        );
    }
    let mut index_line = Vec::new();
    push_json(
        &mut index_line,
        &json!({"id": sid, "thread_name": t.title(), "updated_at": stamp(now)}),
    );
    Rendered {
        rel: format!(
            "sessions/{}/rollout-{}-{sid}.jsonl",
            now.format("%Y/%m/%d"),
            now.format("%Y-%m-%dT%H-%M-%S")
        ),
        session_id: sid,
        bytes: out,
        index_line: Some(index_line),
    }
}

pub fn render(to: ProviderId, t: &Transcript, cwd: &str, now: DateTime<Utc>) -> Result<Rendered> {
    match to {
        ProviderId::Claude => Ok(write_claude(t, cwd, now)),
        ProviderId::Codex => Ok(write_codex(t, cwd, now)),
        other => bail!("converting to {} is not supported yet", other.as_str()),
    }
}

#[derive(Debug, Clone)]
pub struct ConvertRequest {
    pub source: PathBuf,
    pub from: Option<ProviderId>,
    pub to: ProviderId,
    pub target_home: PathBuf,
    pub mappings: BTreeMap<String, String>,
    pub allow_running: bool,
}

#[derive(Debug, Clone)]
enum Write {
    Create { rel: String, bytes: Vec<u8> },
    Append { rel: String, line: Vec<u8> },
}

pub struct PreparedConversion {
    pub plan: Plan,
    pub summary: ConvertSummary,
    target_home: PathBuf,
    source: PathBuf,
    writes: Vec<Write>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConvertSummary {
    pub from: ProviderId,
    pub to: ProviderId,
    pub source_session: String,
    pub session_id: String,
    pub cwd: String,
    pub title: String,
    pub messages: usize,
    pub dropped: Dropped,
    pub already_converted: bool,
    pub resume: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConvertReport {
    #[serde(flatten)]
    pub summary: ConvertSummary,
    pub journal_id: Option<String>,
    pub written: usize,
}

fn read_source(path: &Path) -> Result<Vec<u8>> {
    crate::archive::ensure_regular(path).with_context(|| format!("reading {}", path.display()))?;
    let size = std::fs::metadata(path)?.len();
    if size > MAX_SOURCE_BYTES {
        bail!("session file is larger than {} bytes", MAX_SOURCE_BYTES);
    }
    Ok(std::fs::read(path)?)
}

fn codex_session_exists(home: &Path, session_id: &str) -> bool {
    let suffix = format!("-{session_id}.jsonl");
    walkdir::WalkDir::new(home.join("sessions"))
        .follow_links(false)
        .into_iter()
        .flatten()
        .any(|entry| {
            entry.file_type().is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(&suffix))
        })
}

fn resume_hint(to: ProviderId, cwd: &str, session_id: &str) -> String {
    let quoted = if cwd.contains(char::is_whitespace) {
        format!("\"{cwd}\"")
    } else {
        cwd.to_string()
    };
    match to {
        ProviderId::Codex => format!("cd {quoted} && codex resume {session_id}"),
        _ => format!("cd {quoted} && claude --resume {session_id}"),
    }
}

pub fn prepare(request: &ConvertRequest) -> Result<PreparedConversion> {
    prepare_at(request, Utc::now())
}

pub fn prepare_at(request: &ConvertRequest, now: DateTime<Utc>) -> Result<PreparedConversion> {
    let bytes = read_source(&request.source)?;
    let from = match request.from {
        Some(from) => from,
        None => {
            detect(&bytes).context("could not tell which tool wrote this session; pass --from")?
        }
    };
    if from == request.to {
        bail!("the session already belongs to {}", request.to.as_str());
    }
    let transcript = read(from, &bytes)?;
    let cwd = crate::remap::apply_longest_mapping(&transcript.cwd, &request.mappings);
    if cwd.trim().is_empty() {
        bail!("the session has no working directory; pass a mapping or fix the source");
    }
    let rendered = render(request.to, &transcript, &cwd, now)?;
    let target = crate::engine::checked_dest(&request.target_home, &rendered.rel)?;
    let already = match request.to {
        ProviderId::Codex => codex_session_exists(&request.target_home, &rendered.session_id),
        _ => target.exists(),
    };
    let provider_label = request.to.as_str().to_string();
    let mut plan = Plan::default();
    let mut writes = Vec::new();
    if already {
        plan.ops.push(PlannedOp {
            provider: provider_label.clone(),
            dest: target,
            action: Action::Skip("already converted; run rollback first to convert again".into()),
            risk: Risk::Sensitive,
            size: rendered.bytes.len() as u64,
            rewrites: 0,
            note: String::new(),
        });
    } else {
        let running = request.to == ProviderId::Codex
            && !request.allow_running
            && crate::provider::find(request.to)
                .is_some_and(|p| p.running_hint(&request.target_home).is_some());
        if running {
            plan.ops.push(PlannedOp {
                provider: provider_label.clone(),
                dest: request.target_home.clone(),
                action: Action::Refuse("provider may be running".into()),
                risk: Risk::Normal,
                size: 0,
                rewrites: 0,
                note: String::new(),
            });
        }
        plan.ops.push(PlannedOp {
            provider: provider_label.clone(),
            dest: target,
            action: Action::Create,
            risk: Risk::Sensitive,
            size: rendered.bytes.len() as u64,
            rewrites: 0,
            note: crate::ui::plural(transcript.turns.len(), "message"),
        });
        writes.push(Write::Create {
            rel: rendered.rel.clone(),
            bytes: rendered.bytes.clone(),
        });
        if let Some(line) = &rendered.index_line {
            let index = crate::engine::checked_dest(&request.target_home, CODEX_INDEX)?;
            plan.ops.push(PlannedOp {
                provider: provider_label,
                action: if index.exists() {
                    Action::Merge
                } else {
                    Action::Create
                },
                dest: index,
                risk: Risk::Normal,
                size: line.len() as u64,
                rewrites: 0,
                note: "session index entry".into(),
            });
            writes.push(Write::Append {
                rel: CODEX_INDEX.into(),
                line: line.clone(),
            });
        }
    }
    let dropped = &transcript.dropped;
    if dropped.reasoning > 0 {
        plan.warnings.push(format!(
            "{} reasoning block(s) are bound to the source tool and were not carried over",
            dropped.reasoning
        ));
    }
    if dropped.images > 0 {
        plan.warnings
            .push(format!("{} image(s) were not carried over", dropped.images));
    }
    if dropped.sidechain > 0 {
        plan.warnings.push(format!(
            "{} left out",
            crate::ui::plural(dropped.sidechain, "subagent message")
        ));
    }
    let summary = ConvertSummary {
        from,
        to: request.to,
        source_session: transcript.id.clone(),
        resume: resume_hint(request.to, &cwd, &rendered.session_id),
        session_id: rendered.session_id,
        cwd,
        title: transcript.title(),
        messages: transcript.turns.len(),
        dropped: transcript.dropped.clone(),
        already_converted: already,
    };
    Ok(PreparedConversion {
        plan,
        summary,
        target_home: request.target_home.clone(),
        source: request.source.clone(),
        writes,
    })
}

pub fn apply(prepared: PreparedConversion) -> Result<ConvertReport> {
    apply_with_journal(prepared, crate::journal::Journal::new)
}

pub(crate) fn apply_with_journal(
    prepared: PreparedConversion,
    journal_for: impl FnOnce(&Path) -> Result<crate::journal::Journal>,
) -> Result<ConvertReport> {
    if prepared.plan.has_refusals() {
        bail!("conversion plan contains refused operations");
    }
    if prepared.writes.is_empty() {
        return Ok(ConvertReport {
            summary: prepared.summary,
            journal_id: None,
            written: 0,
        });
    }
    let mut journal = journal_for(&prepared.source)?;
    let mut written = 0;
    for write in &prepared.writes {
        let (rel, bytes, mode) = match write {
            Write::Create { rel, bytes } => (rel, bytes.clone(), WriteMode::CreateNew),
            Write::Append { rel, line } => {
                let dest = crate::engine::checked_dest(&prepared.target_home, rel)?;
                let mut current = if dest.exists() {
                    crate::archive::ensure_regular(&dest)?;
                    std::fs::read(&dest)?
                } else {
                    Vec::new()
                };
                if !current.is_empty() && !current.ends_with(b"\n") {
                    current.push(b'\n');
                }
                current.extend_from_slice(line);
                (rel, current, WriteMode::Replace)
            }
        };
        let dest = crate::engine::checked_dest(&prepared.target_home, rel)?;
        let index = journal.before(&dest, &bytes)?;
        crate::safety::atomic_write(&prepared.target_home, rel, &bytes, mode)?;
        journal.complete(index)?;
        written += 1;
    }
    Ok(ConvertReport {
        summary: prepared.summary,
        journal_id: Some(journal.id().to_string()),
        written,
    })
}

pub fn find_session(
    reference: &str,
    from: Option<ProviderId>,
    homes: &BTreeMap<ProviderId, PathBuf>,
) -> Result<(ProviderId, PathBuf)> {
    let path = Path::new(reference);
    if path.is_file() {
        let bytes = read_source(path)?;
        let provider = match from {
            Some(provider) => provider,
            None => {
                detect(&bytes).context("could not tell which tool wrote this file; pass --from")?
            }
        };
        return Ok((provider, path.to_path_buf()));
    }
    crate::safety::validate_id(reference)
        .context("pass a session id or the path to a session file")?;
    let mut found = Vec::new();
    if from.is_none_or(|p| p == ProviderId::Claude) {
        if let Some(home) = homes.get(&ProviderId::Claude) {
            let projects = home.join("projects");
            for entry in std::fs::read_dir(&projects).into_iter().flatten().flatten() {
                let candidate = entry.path().join(format!("{reference}.jsonl"));
                if candidate.is_file() {
                    found.push((ProviderId::Claude, candidate));
                }
            }
        }
    }
    if from.is_none_or(|p| p == ProviderId::Codex) {
        if let Some(home) = homes.get(&ProviderId::Codex) {
            let suffix = format!("-{reference}.jsonl");
            for entry in walkdir::WalkDir::new(home.join("sessions"))
                .follow_links(false)
                .into_iter()
                .flatten()
            {
                let matches = entry.file_type().is_file()
                    && entry.file_name().to_str().is_some_and(|name| {
                        name.starts_with("rollout-") && name.ends_with(&suffix)
                    });
                if matches {
                    found.push((ProviderId::Codex, entry.into_path()));
                }
            }
        }
    }
    match found.len() {
        0 => bail!("session {reference} was not found in Claude Code or Codex"),
        1 => Ok(found.remove(0)),
        _ => bail!("session {reference} was found more than once; pass the file path or --from"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 9, 12, 0, 0).unwrap()
    }

    fn claude_fixture() -> Vec<u8> {
        let lines = [
            json!({"type":"summary","summary":"x"}),
            json!({"type":"user","sessionId":"c-1","cwd":"/work/app","timestamp":"2026-11-01T10:00:00Z","message":{"role":"user","content":"Fix the login bug"}}),
            json!({"type":"user","sessionId":"c-1","cwd":"/work/app","isMeta":true,"message":{"role":"user","content":"meta line"}}),
            json!({"type":"user","sessionId":"c-1","cwd":"/work/app","message":{"role":"user","content":"<system-reminder>hidden</system-reminder>"}}),
            json!({"type":"assistant","sessionId":"c-1","cwd":"/work/app","timestamp":"2026-11-01T10:00:05Z","message":{"role":"assistant","model":"m","content":[
                {"type":"thinking","thinking":"secret plan","signature":"sig"},
                {"type":"text","text":"Looking at it."},
                {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls ```"}}
            ]}}),
            json!({"type":"user","sessionId":"c-1","cwd":"/work/app","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t1","content":"a.rs\nb.rs","is_error":false},
                {"type":"image","source":{}}
            ]}}),
            json!({"type":"assistant","sessionId":"c-1","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"subagent"}]}}),
            json!({"type":"assistant","sessionId":"c-1","message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"api error"}]}}),
            json!({"type":"assistant","sessionId":"c-1","message":{"role":"assistant","model":"m","content":[{"type":"text","text":"Fixed."}]}}),
        ];
        let mut out = Vec::new();
        for line in lines {
            push_json(&mut out, &line);
        }
        out.extend_from_slice(b"not json\n");
        out
    }

    fn codex_fixture() -> Vec<u8> {
        let lines = [
            json!({"timestamp":"2026-11-01T10:00:00Z","type":"session_meta","payload":{"id":"x-1","cwd":"/work/api","timestamp":"2026-11-01T10:00:00Z"}}),
            json!({"timestamp":"2026-11-01T10:00:00Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"system rules"}]}}),
            json!({"timestamp":"2026-11-01T10:00:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>cwd</environment_context>"}]}}),
            json!({"timestamp":"2026-11-01T10:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Add a cache"},{"type":"input_image","image_url":"data:"}]}}),
            json!({"timestamp":"2026-11-01T10:00:02Z","type":"response_item","payload":{"type":"reasoning","encrypted_content":"zzz","summary":[]}}),
            json!({"timestamp":"2026-11-01T10:00:03Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"cmd\":\"cargo test\"}","call_id":"c1"}}),
            json!({"timestamp":"2026-11-01T10:00:04Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok"}}),
            json!({"timestamp":"2026-11-01T10:00:05Z","type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","input":"*** Begin Patch","call_id":"c2"}}),
            json!({"timestamp":"2026-11-01T10:00:06Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c2","output":{"output":"done"}}}),
            json!({"timestamp":"2026-11-01T10:00:07Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Cache added."}]}}),
            json!({"timestamp":"2026-11-01T10:00:08Z","type":"event_msg","payload":{"type":"token_count"}}),
        ];
        let mut out = Vec::new();
        for line in lines {
            push_json(&mut out, &line);
        }
        out
    }

    #[test]
    fn claude_reader_keeps_the_conversation_and_counts_what_it_drops() {
        let t = read_claude(&claude_fixture()).unwrap();
        assert_eq!(t.id, "c-1");
        assert_eq!(t.cwd, "/work/app");
        assert_eq!(t.turns.len(), 2);
        assert_eq!(
            t.turns[0].parts,
            vec![Part::Text("Fix the login bug".into())]
        );
        assert_eq!(t.turns[1].role, Role::Assistant);
        assert_eq!(t.turns[1].parts.len(), 4);
        assert!(matches!(&t.turns[1].parts[1], Part::ToolCall { name, .. } if name == "Bash"));
        assert!(
            matches!(&t.turns[1].parts[2], Part::ToolResult { output, .. } if output == "a.rs\nb.rs")
        );
        assert_eq!(
            t.dropped,
            Dropped {
                reasoning: 1,
                images: 1,
                injected_context: 1,
                sidechain: 1,
                unreadable_lines: 1
            }
        );
        assert_eq!(t.title(), "Fix the login bug");
    }

    #[test]
    fn codex_reader_handles_both_tool_call_kinds_and_skips_injected_context() {
        let t = read_codex(&codex_fixture()).unwrap();
        assert_eq!(t.id, "x-1");
        assert_eq!(t.cwd, "/work/api");
        assert_eq!(t.turns.len(), 2);
        assert_eq!(t.turns[0].parts, vec![Part::Text("Add a cache".into())]);
        let parts = &t.turns[1].parts;
        assert_eq!(parts.len(), 5);
        assert!(matches!(&parts[3], Part::ToolResult { output, .. } if output.contains("done")));
        assert_eq!(t.dropped.reasoning, 1);
        assert_eq!(t.dropped.images, 1);
        assert_eq!(t.dropped.injected_context, 1);
    }

    #[test]
    fn assistant_text_that_looks_like_markup_is_kept() {
        let line = json!({"type":"assistant","sessionId":"s","cwd":"/w","message":{"role":"assistant","model":"m","content":[{"type":"text","text":"<observation>note</observation>"}]}});
        let t = read_claude(format!("{line}\n").as_bytes()).unwrap();
        assert_eq!(t.turns.len(), 1);
        assert_eq!(t.dropped.injected_context, 0);
        let rendered = write_claude(&t, "/w", now());
        let first: Value = serde_json::from_slice(lines(&rendered.bytes).next().unwrap()).unwrap();
        assert_eq!(first["type"], "user");
    }

    #[test]
    fn detection_tells_the_formats_apart() {
        assert_eq!(detect(&claude_fixture()), Some(ProviderId::Claude));
        assert_eq!(detect(&codex_fixture()), Some(ProviderId::Codex));
        assert_eq!(detect(b"{\"a\":1}\n"), None);
    }

    #[test]
    fn claude_output_is_a_valid_chain_with_a_preamble() {
        let t = read_codex(&codex_fixture()).unwrap();
        let rendered = write_claude(&t, "/srv/api", now());
        assert_eq!(
            rendered.rel,
            format!("projects/-srv-api/{}.jsonl", rendered.session_id)
        );
        let values: Vec<Value> = lines(&rendered.bytes)
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(values.len(), 3);
        assert_eq!(values[0]["parentUuid"], Value::Null);
        assert_eq!(values[1]["parentUuid"], values[0]["uuid"]);
        assert_eq!(values[0]["cwd"], "/srv/api");
        assert_eq!(values[0]["sessionId"], rendered.session_id.as_str());
        let first = values[0]["message"]["content"].as_str().unwrap();
        assert!(first.starts_with("[Imported by aisession-backup from Codex session x-1."));
        assert!(first.ends_with("Add a cache"));
        let answer = values[1]["message"]["content"][0]["text"].as_str().unwrap();
        assert!(answer.contains("Tool call `shell`"));
        assert!(answer.contains("Cache added."));
        assert_eq!(values[2]["type"], "ai-title");
        let back = read_claude(&rendered.bytes).unwrap();
        assert_eq!(back.turns.len(), 2);
    }

    #[test]
    fn codex_output_has_model_and_display_lines_and_reads_back() {
        let t = read_claude(&claude_fixture()).unwrap();
        let rendered = write_codex(&t, "/srv/app", now());
        assert!(rendered
            .rel
            .starts_with("sessions/2026/11/09/rollout-2026-11-09T12-00-00-"));
        assert!(rendered
            .rel
            .ends_with(&format!("-{}.jsonl", rendered.session_id)));
        let values: Vec<Value> = lines(&rendered.bytes)
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(values[0]["type"], "session_meta");
        assert_eq!(values[0]["payload"]["cwd"], "/srv/app");
        let events: Vec<&str> = values
            .iter()
            .filter(|v| v["type"] == "event_msg")
            .map(|v| v["payload"]["type"].as_str().unwrap())
            .collect();
        assert_eq!(events, vec!["user_message", "agent_message"]);
        let shown: Vec<&str> = values
            .iter()
            .filter(|v| v["type"] == "event_msg")
            .map(|v| v["payload"]["message"].as_str().unwrap())
            .collect();
        assert!(shown[0].contains("Fix the login bug"));
        assert!(shown[1].contains("Fixed."));
        let index: Value = serde_json::from_slice(rendered.index_line.as_ref().unwrap()).unwrap();
        assert_eq!(index["id"], rendered.session_id.as_str());
        let back = read_codex(&rendered.bytes).unwrap();
        assert_eq!(back.turns.len(), 2);
        assert!(
            matches!(&back.turns[1].parts[0], Part::Text(text) if text.contains("Tool call `Bash`"))
        );
    }

    #[test]
    fn ids_are_stable_valid_uuids() {
        let a = derived_uuid("seed");
        assert_eq!(a, derived_uuid("seed"));
        assert_ne!(a, derived_uuid("other"));
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn long_tool_output_is_clipped_on_char_boundaries_and_fenced_safely() {
        let text = "é".repeat(TOOL_TEXT_LIMIT + 5);
        let clipped = clip(&text, TOOL_TEXT_LIMIT);
        assert!(clipped.ends_with("[... 5 more characters omitted]"));
        assert_eq!(fence("a ```` b"), "`````");
        let rendered = render_part(&Part::ToolCall {
            name: "x".into(),
            input: "```".into(),
        });
        assert!(rendered.contains("````\n```\n````"));
    }

    #[test]
    fn sessions_without_conversation_or_id_are_rejected() {
        assert!(read_claude(b"{\"type\":\"summary\"}\n").is_err());
        let no_id = json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}});
        assert!(read_codex(format!("{no_id}\n").as_bytes()).is_err());
    }

    fn request(root: &Path, source: PathBuf, to: ProviderId) -> ConvertRequest {
        ConvertRequest {
            source,
            from: None,
            to,
            target_home: root.join("target"),
            mappings: BTreeMap::from([("/work".to_string(), "/srv".to_string())]),
            allow_running: true,
        }
    }

    fn journal(root: &Path) -> impl FnOnce(&Path) -> Result<crate::journal::Journal> + '_ {
        move |source| crate::journal::Journal::at(root.join("journal"), source)
    }

    #[test]
    fn conversion_to_codex_appends_the_index_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("claude.jsonl");
        std::fs::write(&source, claude_fixture()).unwrap();
        let req = request(tmp.path(), source, ProviderId::Codex);
        std::fs::create_dir_all(&req.target_home).unwrap();
        std::fs::write(req.target_home.join(CODEX_INDEX), b"{\"id\":\"old\"}").unwrap();

        let prepared = prepare_at(&req, now()).unwrap();
        assert_eq!(prepared.summary.cwd, "/srv/app");
        assert!(prepared
            .summary
            .resume
            .ends_with(&format!("codex resume {}", prepared.summary.session_id)));
        assert_eq!(prepared.plan.warnings.len(), 3);
        let report = apply_with_journal(prepared, journal(tmp.path())).unwrap();
        assert_eq!(report.written, 2);
        let index = std::fs::read_to_string(req.target_home.join(CODEX_INDEX)).unwrap();
        assert_eq!(index.lines().count(), 2);
        assert!(index.starts_with("{\"id\":\"old\"}\n"));

        let again = prepare_at(&req, now() + chrono::Duration::hours(1)).unwrap();
        assert!(again.summary.already_converted);
        assert!(matches!(again.plan.ops[0].action, Action::Skip(_)));
        assert_eq!(
            apply_with_journal(again, journal(tmp.path()))
                .unwrap()
                .written,
            0
        );
    }

    #[test]
    fn conversion_to_claude_refuses_symlinked_sources_and_same_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("codex.jsonl");
        std::fs::write(&source, codex_fixture()).unwrap();
        let req = request(tmp.path(), source.clone(), ProviderId::Claude);
        let prepared = prepare_at(&req, now()).unwrap();
        assert!(prepared
            .plan
            .ops
            .iter()
            .all(|op| op.action == Action::Create));
        let dest = req.target_home.join(format!(
            "projects/-srv-api/{}.jsonl",
            prepared.summary.session_id
        ));
        apply_with_journal(prepared, journal(tmp.path())).unwrap();
        assert!(dest.is_file());

        let same = request(tmp.path(), source.clone(), ProviderId::Codex);
        assert!(prepare_at(&same, now()).is_err());

        #[cfg(unix)]
        {
            let link = tmp.path().join("link.jsonl");
            std::os::unix::fs::symlink(&source, &link).unwrap();
            assert!(prepare_at(&request(tmp.path(), link, ProviderId::Claude), now()).is_err());
        }
    }

    #[test]
    fn hostile_ids_and_paths_cannot_escape_the_target() {
        let tmp = tempfile::tempdir().unwrap();
        let hostile = json!({"type":"session_meta","payload":{"id":"../../../etc/passwd","cwd":"/work/../../etc"}});
        let message = json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}});
        let source = tmp.path().join("hostile.jsonl");
        std::fs::write(&source, format!("{hostile}\n{message}\n")).unwrap();
        let req = request(tmp.path(), source, ProviderId::Claude);
        let prepared = prepare_at(&req, now()).unwrap();
        for op in &prepared.plan.ops {
            assert!(op.dest.starts_with(&req.target_home), "{:?}", op.dest);
        }
        assert!(find_session("../../etc/passwd", None, &BTreeMap::new()).is_err());
    }
}
