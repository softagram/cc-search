use chrono::{DateTime, Local, TimeZone, Utc};
use clap::Parser;
use colored::Colorize;
use rayon::prelude::*;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

// --- CLI ---

#[derive(Parser)]
#[command(name = "cc-search", about = "Search Claude Code conversation history")]
struct Cli {
    /// Search terms (all must match)
    #[arg(required = true)]
    terms: Vec<String>,

    /// Case-sensitive search (default is case-insensitive)
    #[arg(short = 's', long)]
    case_sensitive: bool,

    /// Max results to show
    #[arg(short = 'n', long, default_value_t = 20)]
    max_results: usize,

    /// Number of matching lines to show per session
    #[arg(short = 'c', long, default_value_t = 3)]
    context_lines: usize,

    /// Only show sessions from this project (substring match)
    #[arg(short = 'p', long)]
    project: Option<String>,

    /// Include agent/subagent sessions
    #[arg(long)]
    include_agents: bool,

    /// Result ordering: most hits first, or most recent match first
    #[arg(long, value_enum, default_value_t = SortBy::Hits)]
    sort: SortBy,

    /// Only count matches newer than this. Accepts a duration (30m, 2h, 7d,
    /// 1h30m), an RFC3339 timestamp (2026-10-04T08:00:00Z), a date
    /// (2026-10-04) or Unix seconds - like `docker logs --since`.
    #[arg(long, value_name = "WHEN")]
    since: Option<String>,

    /// Also match tool calls, tool output and session titles, not just prose
    #[arg(long)]
    tools: bool,

    /// Match anything in the transcript, including auto-injected boilerplate
    /// (skill listings, CLAUDE.md, system reminders, hook output)
    #[arg(long)]
    all: bool,

    /// Custom Claude config directory
    #[arg(long)]
    claude_dir: Option<PathBuf>,
}

#[derive(clap::ValueEnum, Clone, Copy, PartialEq, Eq, Debug)]
enum SortBy {
    /// Most matches first - finds where the work on a topic happened.
    Hits,
    /// Most recent match first.
    ///
    /// Keyed on when the term was last *mentioned*, not when the session was
    /// last active. Those differ: on one archive 39 of 135 matching sessions
    /// had a gap over a day between the two, and 17 over a week (worst case
    /// 34 days), because a long-running session touched yesterday makes a
    /// month-old mention look fresh.
    Recency,
}

// --- --since parsing ---

/// Parse a `--since` value into an absolute instant, `docker logs` style.
///
/// Accepts a duration relative to `now` (`30m`, `2h`, `7d`, `1h30m`), an
/// RFC3339 timestamp, a bare date (local midnight), or Unix seconds.
///
/// Unlike Go's `time.ParseDuration`, which docker uses, `d` (days) and `w`
/// (weeks) are accepted: a conversation archive is searched in days and weeks,
/// and `--since 7d` beats making the user write `168h`.
fn parse_since(value: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    let v = value.trim();
    if v.is_empty() {
        return Err("empty --since value".to_string());
    }

    // Unix seconds: all digits, and long enough not to be a bare duration.
    if v.len() >= 9 && v.bytes().all(|b| b.is_ascii_digit()) {
        let secs: i64 = v.parse().map_err(|_| format!("bad Unix timestamp: {v}"))?;
        return DateTime::from_timestamp(secs, 0)
            .ok_or_else(|| format!("Unix timestamp out of range: {v}"));
    }

    // RFC3339 timestamp, e.g. 2026-10-04T08:00:00Z
    if let Ok(dt) = v.parse::<DateTime<Utc>>() {
        return Ok(dt);
    }

    // Bare date, e.g. 2026-10-04 -> local midnight, so "--since 2026-10-04"
    // means what the user's calendar says rather than what UTC says.
    if let Ok(date) = chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d") {
        let naive = date.and_hms_opt(0, 0, 0).expect("midnight is a valid time");
        return match Local.from_local_datetime(&naive).earliest() {
            Some(local) => Ok(local.with_timezone(&Utc)),
            None => Err(format!("ambiguous local date: {v}")),
        };
    }

    // Duration, possibly compound: 1h30m, 2h, 7d, 90s, 3w
    parse_duration(v)
        .map(|secs| now - chrono::Duration::seconds(secs))
        .ok_or_else(|| {
            format!(
                "cannot parse --since {v:?}; expected a duration (30m, 2h, 7d, 1h30m), \
                 an RFC3339 timestamp, a date (YYYY-MM-DD) or Unix seconds"
            )
        })
}

/// Sum a compound duration like `1h30m` into seconds. Returns None on any
/// unknown unit, missing unit, or empty input - no silent partial parses.
fn parse_duration(v: &str) -> Option<i64> {
    let mut total: i64 = 0;
    let mut digits = String::new();
    let mut saw_unit = false;

    for ch in v.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            continue;
        }
        if digits.is_empty() {
            return None; // unit with no number in front of it
        }
        let n: i64 = digits.parse().ok()?;
        digits.clear();
        let mult = match ch {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            'w' => 604_800,
            _ => return None,
        };
        total = total.checked_add(n.checked_mul(mult)?)?;
        saw_unit = true;
    }

    // Trailing digits mean a number with no unit, e.g. "30" or "1h30".
    if !digits.is_empty() || !saw_unit {
        return None;
    }
    Some(total)
}

// --- Data model ---

#[derive(Deserialize)]
struct JsonlEntry {
    #[serde(rename = "type")]
    entry_type: Option<String>,
    #[allow(dead_code)]
    subtype: Option<String>,
    message: Option<MessageData>,
    timestamp: Option<String>,
    #[allow(dead_code)]
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
}

#[derive(Deserialize)]
struct MessageData {
    role: Option<String>,
    content: Option<serde_json::Value>,
}

struct SessionInfo {
    session_id: String,
    project: String,
    #[allow(dead_code)]
    file_path: PathBuf,
    cwd: Option<String>,
    custom_title: Option<String>,
    first_user_msg: Option<MessageSnippet>,
    last_user_msg: Option<MessageSnippet>,
    #[allow(dead_code)]
    last_assistant_msg: Option<MessageSnippet>,
    first_timestamp: Option<DateTime<Utc>>,
    last_timestamp: Option<DateTime<Utc>>,
    user_msg_count: usize,
    assistant_msg_count: usize,
    total_entries: usize,
    compacted: bool,
    compaction_count: usize,
    matching_lines: Vec<MatchLine>,
    hit_counts: HitCounts,
    /// Timestamp of the newest qualifying hit - the key `--sort recency` uses.
    last_hit: Option<DateTime<Utc>>,
}

/// Every hit in a session, counted by kind. Uncapped, unlike the handful of
/// excerpts actually displayed - the totals are what tell you whether a
/// session is *about* the term or merely mentions it once.
#[derive(Default, Clone, Copy)]
struct HitCounts {
    user: usize,
    assistant: usize,
    tool: usize,
    noise: usize,
}

impl HitCounts {
    fn prose(&self) -> usize {
        self.user + self.assistant
    }

    /// Hits that count towards a match at the given tier.
    fn at_or_above(&self, tier: Tier) -> usize {
        match tier {
            Tier::Prose => self.prose(),
            Tier::Tool => self.prose() + self.tool,
            Tier::Noise => self.prose() + self.tool + self.noise,
        }
    }

    fn add(&mut self, tier: Tier, label: &str) {
        match tier {
            Tier::Prose if label == "user" => self.user += 1,
            Tier::Prose => self.assistant += 1,
            Tier::Tool => self.tool += 1,
            Tier::Noise => self.noise += 1,
        }
    }

    fn merge(&mut self, other: &HitCounts) {
        self.user += other.user;
        self.assistant += other.assistant;
        self.tool += other.tool;
        self.noise += other.noise;
    }

    /// "12 user, 9 asst, 3 tool" - omits kinds with no hits.
    fn describe(&self, tier: Tier) -> String {
        let mut parts = Vec::new();
        if self.user > 0 {
            parts.push(format!("{} user", self.user));
        }
        if self.assistant > 0 {
            parts.push(format!("{} asst", self.assistant));
        }
        if tier <= Tier::Tool && self.tool > 0 {
            parts.push(format!("{} tool", self.tool));
        }
        if tier == Tier::Noise && self.noise > 0 {
            parts.push(format!("{} boilerplate", self.noise));
        }
        parts.join(", ")
    }
}

struct MessageSnippet {
    text: String,
    timestamp: Option<DateTime<Utc>>,
}

struct MatchLine {
    role: String,
    text: String,
}

// --- Ripgrep search ---

fn find_rg() -> String {
    if cfg!(target_os = "windows") {
        // On Windows, rg is typically in PATH via scoop/choco/winget
        for path in &[
            r"C:\ProgramData\chocolatey\bin\rg.exe",
        ] {
            if Path::new(path).exists() {
                return path.to_string();
            }
        }
        "rg.exe".to_string()
    } else {
        for path in &[
            "/opt/homebrew/bin/rg",
            "/usr/local/bin/rg",
            "/usr/bin/rg",
        ] {
            if Path::new(path).exists() {
                return path.to_string();
            }
        }
        "rg".to_string()
    }
}

fn ripgrep_search(search_dir: &Path, terms: &[String], ignore_case: bool) -> Vec<PathBuf> {
    let rg = find_rg();
    let first_term = &terms[0];

    let mut cmd = Command::new(&rg);
    cmd.arg("--files-with-matches")
        .arg("--no-messages")
        .arg("--glob")
        .arg("*.jsonl");

    if ignore_case {
        cmd.arg("-i");
    }

    cmd.arg(first_term).arg(search_dir);

    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{}: rg not found: {}", "error".red().bold(), e);
            std::process::exit(1);
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut files: Vec<PathBuf> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect();

    // Filter by additional terms using rg
    for term in &terms[1..] {
        if files.is_empty() {
            break;
        }

        let file_list: Vec<String> = files
            .iter()
            .map(|f| f.to_string_lossy().into_owned())
            .collect();

        let mut cmd2 = Command::new(&rg);
        cmd2.arg("--files-with-matches").arg("--no-messages");
        if ignore_case {
            cmd2.arg("-i");
        }
        cmd2.arg(term);
        for f in &file_list {
            cmd2.arg(f);
        }

        let output2 = match cmd2.output() {
            Ok(o) => o,
            Err(_) => return vec![],
        };

        let stdout2 = String::from_utf8_lossy(&output2.stdout);
        files = stdout2
            .lines()
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect();
    }

    files
}

// --- Match tiers ---
//
// A session .jsonl holds far more than the conversation: Claude Code injects
// the skill listing, CLAUDE.md, hook output, token reminders and file-history
// snapshots into every transcript. A plain substring hit on the file therefore
// says nothing about whether the session was *about* the term - on one real
// archive, searching for a customer name matched 978 files, 824 of which only
// contained it inside an auto-injected `create-acme-invoice` skill description.
//
// Every entry is classified into a tier, and a session only counts as a match
// if it has a hit at or above the requested tier.

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Tier {
    /// Auto-injected boilerplate: skill listings, CLAUDE.md, system reminders,
    /// hook output, token reminders, file-history snapshots.
    Noise = 0,
    /// What the session *did*: tool calls, tool output, session titles.
    Tool = 1,
    /// What was actually said: user prose, assistant prose, thinking.
    Prose = 2,
}

/// Lowest tier a hit must reach for a session to be reported.
///
/// Prose is the default: the question a history search answers is "where did
/// we talk about this?", and tool output is full of incidental mentions - an
/// `ls` of a directory whose name contains the term matches every time.
/// Widen deliberately with --tools, or --all for a literal file search.
fn select_min_tier(all: bool, tools: bool) -> Tier {
    match (all, tools) {
        (true, _) => Tier::Noise,
        (false, true) => Tier::Tool,
        (false, false) => Tier::Prose,
    }
}

/// Why a ripgrep candidate did not become a result. The two reasons need
/// different advice, so they must not be lumped together: widening the tier
/// cannot bring back a session excluded by `--since`.
enum Dropped {
    /// No match at all inside the --since window.
    OutOfWindow,
    /// Matched in the window, but only below the requested tier.
    BelowTier,
}

/// A searchable fragment of one transcript entry.
struct Fragment {
    tier: Tier,
    /// Display label, e.g. "user", "assistant", "tool", "skill_listing".
    label: String,
    text: String,
}

/// Attachment kinds that carry content the human chose to bring in, rather
/// than boilerplate the harness injects on every turn.
const MEANINGFUL_ATTACHMENTS: &[&str] = &[
    "file",
    "edited_text_file",
    "queued_command",
    "selected_lines_in_ide",
    "new_diagnostics",
];

/// Remove `<system-reminder>...</system-reminder>` spans. These are injected
/// into user turns by the harness and routinely carry the whole skill listing
/// and CLAUDE.md, so a hit inside one is not a hit on anything the user wrote.
fn strip_system_reminders(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(OPEN) {
        out.push_str(&rest[..i]);
        rest = match rest[i..].find(CLOSE) {
            Some(j) => &rest[i + j + CLOSE.len()..],
            // Unterminated reminder: drop everything after it.
            None => "",
        };
    }
    out.push_str(rest);
    out
}

fn content_to_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// Split one transcript entry into searchable fragments with their tiers.
fn classify_entry(raw: &serde_json::Value) -> Vec<Fragment> {
    let entry_type = raw.get("type").and_then(|v| v.as_str()).unwrap_or("");

    // Session titles describe the session as a whole - useful, but generated.
    for key in ["customTitle", "aiTitle"] {
        if let Some(t) = raw.get(key).and_then(|v| v.as_str()) {
            return vec![Fragment {
                tier: Tier::Tool,
                label: "title".to_string(),
                text: t.to_string(),
            }];
        }
    }

    if entry_type == "attachment" {
        let kind = raw
            .get("attachment")
            .and_then(|a| a.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("attachment");
        let tier = if MEANINGFUL_ATTACHMENTS.contains(&kind) {
            Tier::Tool
        } else {
            Tier::Noise
        };
        let text = raw
            .get("attachment")
            .map(|a| a.to_string())
            .unwrap_or_default();
        return vec![Fragment {
            tier,
            label: kind.to_string(),
            text,
        }];
    }

    if entry_type != "user" && entry_type != "assistant" {
        // system, file-history-snapshot, last-prompt, queue-operation, ...
        return vec![Fragment {
            tier: Tier::Noise,
            label: entry_type.to_string(),
            text: raw.to_string(),
        }];
    }

    let Some(content) = raw.get("message").and_then(|m| m.get("content")) else {
        return vec![];
    };

    let prose_label = entry_type.to_string();
    let mut out = Vec::new();

    match content {
        serde_json::Value::String(s) => {
            let stripped = strip_system_reminders(s);
            let was_stripped = stripped.len() != s.len();
            if !stripped.trim().is_empty() {
                out.push(Fragment {
                    tier: Tier::Prose,
                    label: prose_label.clone(),
                    text: stripped,
                });
            }
            if was_stripped {
                out.push(Fragment {
                    tier: Tier::Noise,
                    label: "system-reminder".to_string(),
                    text: s.clone(),
                });
            }
        }
        serde_json::Value::Array(blocks) => {
            for block in blocks {
                let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match btype {
                    "text" | "thinking" => {
                        let key = if btype == "text" { "text" } else { "thinking" };
                        let Some(raw_text) = block.get(key).and_then(|v| v.as_str()) else {
                            continue;
                        };
                        let stripped = strip_system_reminders(raw_text);
                        if !stripped.trim().is_empty() {
                            out.push(Fragment {
                                tier: Tier::Prose,
                                label: prose_label.clone(),
                                text: stripped.clone(),
                            });
                        }
                        if stripped.len() != raw_text.len() {
                            out.push(Fragment {
                                tier: Tier::Noise,
                                label: "system-reminder".to_string(),
                                text: raw_text.to_string(),
                            });
                        }
                    }
                    "tool_use" => {
                        let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                        let input = block.get("input").map(|i| i.to_string()).unwrap_or_default();
                        out.push(Fragment {
                            tier: Tier::Tool,
                            label: format!("tool:{}", name),
                            text: input,
                        });
                    }
                    "tool_result" => {
                        let text = block.get("content").map(content_to_text).unwrap_or_default();
                        out.push(Fragment {
                            tier: Tier::Tool,
                            label: "tool-out".to_string(),
                            text,
                        });
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    out
}

fn fragment_matches(text: &str, terms: &[String], ignore_case: bool) -> bool {
    if ignore_case {
        let hay = text.to_lowercase();
        terms.iter().all(|t| hay.contains(&t.to_lowercase()))
    } else {
        terms.iter().all(|t| text.contains(t.as_str()))
    }
}

// --- Session parsing ---

fn extract_text_content(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => {
            let mut parts = Vec::new();
            for item in arr {
                if let Some(obj) = item.as_object() {
                    let content_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match content_type {
                        "text" => {
                            if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
                                parts.push(text.to_string());
                            }
                        }
                        "tool_use" => {
                            if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
                                parts.push(format!("[tool: {}]", name));
                            }
                        }
                        _ => {}
                    }
                }
            }
            parts.join(" ")
        }
        _ => String::new(),
    }
}

fn parse_session(
    file: &Path,
    projects_dir: &Path,
    terms: &[String],
    ignore_case: bool,
    context_lines: usize,
    min_tier: Tier,
    since: Option<DateTime<Utc>>,
) -> Result<SessionInfo, Dropped> {
    // Session files can be nested below the project directory (subagent logs
    // live in <project>/<session-uuid>/subagents/agent-*.jsonl), so the project
    // is the first path component under the projects directory — not the
    // file's immediate parent.
    let project = file
        .strip_prefix(projects_dir)
        .ok()
        .and_then(|rel| rel.components().next())
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .or_else(|| Some(file.parent()?.file_name()?.to_string_lossy().to_string()))
        .ok_or(Dropped::BelowTier)?;

    let session_id = file
        .file_stem()
        .ok_or(Dropped::BelowTier)?
        .to_string_lossy()
        .to_string();

    let content = std::fs::read_to_string(file).map_err(|_| Dropped::BelowTier)?;

    let mut first_user_msg: Option<MessageSnippet> = None;
    let mut last_user_msg: Option<MessageSnippet> = None;
    let mut last_assistant_msg: Option<MessageSnippet> = None;
    let mut first_timestamp: Option<DateTime<Utc>> = None;
    let mut last_timestamp: Option<DateTime<Utc>> = None;
    let mut user_msg_count = 0usize;
    let mut assistant_msg_count = 0usize;
    let mut total_entries = 0usize;
    let mut compacted = false;
    let mut compaction_count = 0usize;
    let mut custom_title: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut cwd: Option<String> = None;

    // Best tier seen anywhere in the session, plus the fragments to display.
    let mut best_tier = Tier::Noise;
    let mut hits: Vec<(Tier, MatchLine)> = Vec::new();
    let mut hit_counts = HitCounts::default();
    let mut last_hit: Option<DateTime<Utc>> = None;
    let mut matched_in_window = false;

    for line in content.lines() {
        let Ok(raw) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let entry: JsonlEntry = match serde_json::from_value(raw.clone()) {
            Ok(e) => e,
            Err(_) => continue,
        };

        total_entries += 1;

        let ts = entry
            .timestamp
            .as_ref()
            .and_then(|t| t.parse::<DateTime<Utc>>().ok());

        if let Some(t) = ts {
            if first_timestamp.is_none_or(|ft| t < ft) {
                first_timestamp = Some(t);
            }
            if last_timestamp.is_none_or(|lt| t > lt) {
                last_timestamp = Some(t);
            }
        }

        if entry.entry_type.as_deref() == Some("custom-title")
            && let Some(title) = &entry.custom_title
        {
            custom_title = Some(title.clone());
        }

        if entry.entry_type.as_deref() == Some("ai-title")
            && let Some(title) = &entry.ai_title
        {
            ai_title = Some(title.clone());
        }

        // Extract cwd
        if cwd.is_none()
            && let Some(c) = raw.get("cwd").and_then(|v| v.as_str())
        {
            cwd = Some(c.to_string());
        }

        // Collect matches from this entry, tier by tier. A cheap whole-line
        // pre-check keeps the per-fragment work off the ~99% of lines that
        // cannot match at all.
        // A --since window applies to the match, not the session: the unit of
        // output is a session, but what qualifies it is a mention inside the
        // window. An entry with no timestamp cannot be placed, so it is
        // excluded while a window is active rather than silently assumed
        // in-range.
        let in_window = match since {
            None => true,
            Some(cutoff) => ts.is_some_and(|t| t >= cutoff),
        };

        if in_window && fragment_matches(line, terms, ignore_case) {
            for frag in classify_entry(&raw) {
                if !fragment_matches(&frag.text, terms, ignore_case) {
                    continue;
                }
                matched_in_window = true;
                if frag.tier > best_tier {
                    best_tier = frag.tier;
                }
                hit_counts.add(frag.tier, &frag.label);
                if frag.tier >= min_tier
                    && let Some(t) = ts
                    && last_hit.is_none_or(|prev| t > prev)
                {
                    last_hit = Some(t);
                }
                if frag.tier >= min_tier && hits.len() < context_lines * 8 {
                    hits.push((
                        frag.tier,
                        MatchLine {
                            role: frag.label,
                            text: find_match_context(&frag.text, terms, ignore_case, 150),
                        },
                    ));
                }
            }
        }

        let Some(ref msg) = entry.message else {
            continue;
        };
        let role = msg.role.as_deref().unwrap_or("");
        let text = msg
            .content
            .as_ref()
            .map(extract_text_content)
            .unwrap_or_default();

        match role {
            "user" => {
                user_msg_count += 1;

                if text.contains("This session is being continued from a previous conversation") {
                    compacted = true;
                    compaction_count += 1;
                    continue; // Don't use compaction message as first/last
                }

                if text.is_empty()
                    || text.starts_with("<task-notification>")
                    || text.starts_with("<system-reminder>")
                {
                    continue;
                }

                let snippet = MessageSnippet {
                    text: truncate_clean(&text, 200),
                    timestamp: ts,
                };

                if first_user_msg.is_none() {
                    first_user_msg = Some(snippet);
                } else {
                    last_user_msg = Some(snippet);
                }
            }
            "assistant" => {
                assistant_msg_count += 1;
                if !text.is_empty() {
                    last_assistant_msg = Some(MessageSnippet {
                        text: truncate_clean(&text, 200),
                        timestamp: ts,
                    });
                }
            }
            _ => {}
        }
    }

    // The file matched as raw text, but nothing at the requested tier did -
    // the term only appears in boilerplate. Drop the session entirely rather
    // than presenting an auto-injected skill listing as a "match".
    if !matched_in_window {
        // With no window active, this means ripgrep matched raw bytes that no
        // classified fragment contains - JSON escaping, or a field the
        // classifier does not treat as searchable. That is a tier miss, not a
        // time miss, and must not be reported as one.
        return Err(if since.is_some() {
            Dropped::OutOfWindow
        } else {
            Dropped::BelowTier
        });
    }
    if best_tier < min_tier {
        return Err(Dropped::BelowTier);
    }

    // Show the strongest evidence first: prose over tool traffic over noise.
    hits.sort_by_key(|h| std::cmp::Reverse(h.0));
    let matching_lines: Vec<MatchLine> =
        hits.into_iter().map(|(_, m)| m).take(context_lines).collect();

    Ok(SessionInfo {
        session_id,
        project,
        file_path: file.to_path_buf(),
        cwd,
        custom_title: custom_title.or(ai_title),
        first_user_msg,
        last_user_msg,
        last_assistant_msg,
        first_timestamp,
        last_timestamp,
        user_msg_count,
        assistant_msg_count,
        total_entries,
        compacted,
        compaction_count,
        matching_lines,
        hit_counts,
        last_hit,
    })
}

fn find_match_context(text: &str, terms: &[String], ignore_case: bool, window: usize) -> String {
    let search_text = if ignore_case {
        text.to_lowercase()
    } else {
        text.to_string()
    };

    let first_term = if ignore_case {
        terms[0].to_lowercase()
    } else {
        terms[0].clone()
    };

    let pos = search_text.find(&first_term).unwrap_or(0);

    let start = pos.saturating_sub(window / 2);
    let end = (pos + first_term.len() + window / 2).min(text.len());

    // Align to char boundaries
    let start = floor_char_boundary(text, start);
    let end = ceil_char_boundary(text, end);

    let mut snippet: String = text[start..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    if start > 0 {
        snippet = format!("...{}", snippet);
    }
    if end < text.len() {
        snippet = format!("{}...", snippet);
    }

    snippet
}

/// Safe floor_char_boundary for stable Rust
fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Safe ceil_char_boundary for stable Rust
fn ceil_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

// --- Display ---

fn format_timestamp(ts: &DateTime<Utc>) -> String {
    let local: DateTime<Local> = (*ts).into();
    local.format("%Y-%m-%d %H:%M").to_string()
}

fn format_duration(first: &DateTime<Utc>, last: &DateTime<Utc>) -> String {
    let dur = *last - *first;
    let mins = dur.num_minutes();
    if mins < 60 {
        format!("{}min", mins)
    } else if mins < 1440 {
        format!("{}h {}min", mins / 60, mins % 60)
    } else {
        format!("{}d {}h", mins / 1440, (mins % 1440) / 60)
    }
}

/// Render one session card as lines. Returns them rather than printing, so
/// the caller decides what happens when stdout goes away mid-write.
fn display_session(session: &SessionInfo, index: usize, min_tier: Tier) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    lines.push("─".repeat(80).dimmed().to_string());

    // Header
    let title = session.custom_title.as_deref().unwrap_or("(no title)");
    lines.push(format!(
        "{}  {}",
        format!("#{}", index + 1).bold().cyan(),
        title.bold().white()
    ));

    // Project + session ID
    lines.push(format!(
        "  {}  {}",
        pretty_project(&session.project).green(),
        session.session_id.dimmed()
    ));

    // CWD if different from project
    if let Some(ref cwd) = session.cwd {
        lines.push(format!("  {}", cwd.dimmed()));
    }

    // Timestamps + duration
    if let (Some(first), Some(last)) = (session.first_timestamp, session.last_timestamp) {
        lines.push(format!(
            "  {} {} {} {}  {}",
            "from".dimmed(),
            format_timestamp(&first).yellow(),
            "to".dimmed(),
            format_timestamp(&last).yellow(),
            format!("({})", format_duration(&first, &last)).dimmed()
        ));
    }

    // Stats
    let compact_str = if session.compacted {
        if session.compaction_count > 1 {
            format!("COMPACTED ({}x)", session.compaction_count)
                .red()
                .bold()
                .to_string()
        } else {
            "COMPACTED".red().bold().to_string()
        }
    } else {
        "not compacted".dimmed().to_string()
    };
    lines.push(format!(
        "  {} user / {} assistant msgs | {} entries | {}",
        session.user_msg_count.to_string().bold(),
        session.assistant_msg_count.to_string().bold(),
        session.total_entries,
        compact_str
    ));

    // First / last user message
    for (label, msg) in [
        ("FIRST:", &session.first_user_msg),
        ("LAST: ", &session.last_user_msg),
    ] {
        if let Some(msg) = msg {
            let ts_str = msg
                .timestamp
                .as_ref()
                .map(format_timestamp)
                .unwrap_or_default();
            lines.push(format!(
                "  {} {} {}",
                label.blue().bold(),
                ts_str.dimmed(),
                msg.text
            ));
        }
    }

    // Matching lines, headed by the full hit count for this session
    if !session.matching_lines.is_empty() {
        let shown = session.matching_lines.len();
        let total = session.hit_counts.at_or_above(min_tier);
        let noun = if total == 1 { "hit" } else { "hits" };
        let breakdown = session.hit_counts.describe(min_tier);
        let suffix = if total > shown {
            format!("{} {} ({}) - showing {}", total, noun, breakdown, shown)
        } else {
            format!("{} {} ({})", total, noun, breakdown)
        };
        lines.push(format!(
            "  {} {}",
            "MATCHES:".magenta().bold(),
            format!("[{}]", suffix).dimmed()
        ));
        for m in &session.matching_lines {
            let role_tag = match m.role.as_str() {
                "user" => "user".cyan().to_string(),
                "assistant" => "asst".green().to_string(),
                "title" => "title".blue().to_string(),
                "tool-out" => "tool-out".yellow().to_string(),
                other if other.starts_with("tool:") => other.yellow().to_string(),
                other => other.dimmed().to_string(),
            };
            lines.push(format!("    [{}] {}", role_tag, m.text));
        }
    }

    // Resume command
    let resume_cwd = session.cwd.as_deref().unwrap_or("~");
    lines.push(if cfg!(target_os = "windows") {
        format!(
            "  {} cd /d {} & claude --resume {}",
            "RESUME:".dimmed(),
            resume_cwd,
            session.session_id.dimmed()
        )
    } else {
        format!(
            "  {} cd {} && claude --resume {}",
            "RESUME:".dimmed(),
            resume_cwd,
            session.session_id.dimmed()
        )
    });

    lines
}

/// Write lines to stdout, reporting whether the reader is still there.
///
/// `cc-search ... | head -40` closes the pipe as soon as head has its 40
/// lines. Rust ignores SIGPIPE and turns the resulting EPIPE into a panic
/// from `println!`, which would splatter a backtrace across the results -
/// so writes are checked and a closed pipe simply ends the listing.
fn write_lines(lines: &[String]) -> bool {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in lines {
        if writeln!(out, "{}", line).is_err() {
            return false;
        }
    }
    out.flush().is_ok()
}

fn pretty_project(raw: &str) -> String {
    // Claude Code encodes the project path as the directory name with path
    // separators replaced by dashes. Strip the home directory prefix to get
    // a human-readable project name.
    //
    // macOS: -Users-jane-code-myapp → myapp
    // Linux: -home-jane-code-myapp  → myapp
    // Windows: -C-Users-jane-code-myapp → myapp
    let home = dirs_home();
    let home_str = home.to_string_lossy().replace(['/', '\\'], "-");
    // The prefix is "-" + home path with separators as dashes + "-"
    // e.g. home=/Users/jane → prefix="-Users-jane-"
    let prefix = if home_str.starts_with('-') {
        format!("{}-", home_str)
    } else {
        format!("-{}-", home_str)
    };

    let stripped = raw.strip_prefix(&prefix).unwrap_or(raw);

    // Further strip common subdirectory prefixes like "code-", "tools-", etc.
    stripped
        .strip_prefix("code-")
        .or_else(|| stripped.strip_prefix("tools-"))
        .or_else(|| stripped.strip_prefix("projects-"))
        .unwrap_or(stripped)
        .to_string()
}

fn truncate_clean(s: &str, max_chars: usize) -> String {
    // Collapse whitespace and truncate
    let cleaned: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.len() <= max_chars {
        return cleaned;
    }
    let end = floor_char_boundary(&cleaned, max_chars);
    format!("{}...", &cleaned[..end])
}

// --- Main ---

fn main() {
    let cli = Cli::parse();

    let claude_dir = cli.claude_dir.unwrap_or_else(|| dirs_home().join(".claude"));

    let projects_dir = claude_dir.join("projects");

    if !projects_dir.exists() {
        eprintln!(
            "{}: Claude projects directory not found: {}",
            "error".red().bold(),
            projects_dir.display()
        );
        std::process::exit(1);
    }

    let ignore_case = !cli.case_sensitive;

    let min_tier = select_min_tier(cli.all, cli.tools);

    let since = match cli.since.as_deref() {
        None => None,
        Some(v) => match parse_since(v, Utc::now()) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!("{}: {}", "error".red().bold(), e);
                std::process::exit(2);
            }
        },
    };

    // Filter by project if specified
    let search_dirs: Vec<PathBuf> = if let Some(ref proj_filter) = cli.project {
        match std::fs::read_dir(&projects_dir) {
            Ok(entries) => entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .filter(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let pretty = pretty_project(&name);
                    pretty.contains(proj_filter.as_str()) || name.contains(proj_filter.as_str())
                })
                .map(|e| e.path())
                .collect(),
            Err(_) => vec![],
        }
    } else {
        vec![projects_dir.clone()]
    };

    if search_dirs.is_empty() {
        eprintln!(
            "{}: No projects matching '{}'",
            "error".red().bold(),
            cli.project.as_deref().unwrap_or("")
        );
        std::process::exit(1);
    }

    // Search
    eprintln!(
        "{} Searching for: {}",
        ">>".blue().bold(),
        cli.terms.join(" + ").bold()
    );

    let mut all_matches: Vec<PathBuf> = Vec::new();
    for dir in &search_dirs {
        let matches = ripgrep_search(dir, &cli.terms, ignore_case);
        all_matches.extend(matches);
    }

    // Filter agent sessions unless requested
    if !cli.include_agents {
        all_matches.retain(|f| {
            let fname = f.file_name().unwrap_or_default().to_string_lossy();
            !fname.starts_with("agent-")
        });
    }

    let candidate_count = all_matches.len();

    if all_matches.is_empty() {
        eprintln!("{} Found 0 matching sessions", ">>".blue().bold());
        return;
    }

    // Parse in parallel. parse_session drops candidates whose only hit is
    // below min_tier, so this is where boilerplate-only files fall away.
    let outcomes: Vec<Result<SessionInfo, Dropped>> = all_matches
        .par_iter()
        .map(|f| {
            parse_session(
                f,
                &projects_dir,
                &cli.terms,
                ignore_case,
                cli.context_lines,
                min_tier,
                since,
            )
        })
        .collect();

    let mut dropped = DropCounts::default();
    let mut sessions: Vec<SessionInfo> = Vec::with_capacity(outcomes.len());
    for outcome in outcomes {
        match outcome {
            Ok(s) => sessions.push(s),
            Err(Dropped::OutOfWindow) => dropped.out_of_window += 1,
            Err(Dropped::BelowTier) => dropped.below_tier += 1,
        }
    }
    debug_assert_eq!(candidate_count, sessions.len() + dropped.total());

    if sessions.is_empty() {
        eprintln!("{} Found 0 matching sessions", ">>".blue().bold());
        if dropped.total() > 0 {
            eprintln!("{} {}", ">>".blue().bold(), widen_hint(min_tier, dropped));
        }
        return;
    }

    match cli.sort {
        // Ties broken by recency of the match, so equal-hit sessions are
        // still in a useful order rather than filesystem order.
        SortBy::Hits => sessions.sort_by_key(|s| {
            (
                std::cmp::Reverse(s.hit_counts.at_or_above(min_tier)),
                std::cmp::Reverse(s.last_hit),
            )
        }),
        SortBy::Recency => sessions.sort_by_key(|s| std::cmp::Reverse(s.last_hit)),
    }

    let show_count = sessions.len().min(cli.max_results);

    // The banner goes to stderr and is printed both before and after the
    // results, so it survives `cc-search ... | head -40`: stdout is truncated
    // by head, stderr is not. Without this, piping hides exactly the numbers
    // you need to decide whether to re-sort or widen.
    let banner = summary_banner(&sessions, min_tier, cli.sort, show_count, dropped, since);
    for line in &banner {
        eprintln!("{} {}", ">>".blue().bold(), line);
    }

    for (i, session) in sessions.iter().take(show_count).enumerate() {
        if !write_lines(&display_session(session, i, min_tier)) {
            break;
        }
    }
    write_lines(&["─".repeat(80).dimmed().to_string()]);

    // Repeated on stderr, so it survives the pipe that just closed.
    for line in &banner {
        eprintln!("{} {}", ">>".blue().bold(), line);
    }
}

#[derive(Default, Clone, Copy)]
struct DropCounts {
    out_of_window: usize,
    below_tier: usize,
}

impl DropCounts {
    fn total(&self) -> usize {
        self.out_of_window + self.below_tier
    }
}

/// Hint naming the flag that would reveal the sessions filtered out.
fn widen_hint(min_tier: Tier, dropped: DropCounts) -> String {
    let mut parts = Vec::new();

    if dropped.out_of_window > 0 {
        parts.push(format!(
            "{} outside the --since window",
            dropped.out_of_window
        ));
    }

    if dropped.below_tier > 0 {
        let (where_, flag) = match min_tier {
            Tier::Prose => ("tool calls, tool output or boilerplate", "--tools / --all"),
            Tier::Tool => ("injected boilerplate (skill listings, CLAUDE.md)", "--all"),
            // Nothing is below the widest tier, so nothing can land here.
            Tier::Noise => ("no qualifying match", "--since to widen"),
        };
        parts.push(format!(
            "{} matching only in {} ({} to widen)",
            dropped.below_tier, where_, flag
        ));
    }

    if parts.is_empty() {
        return String::new();
    }
    format!("Hid {}: {}", dropped.total(), parts.join("; "))
}

/// The stderr banner: what was found, what kind of hits, how it is ordered,
/// and which flag changes each of those.
fn summary_banner(
    sessions: &[SessionInfo],
    min_tier: Tier,
    sort: SortBy,
    show_count: usize,
    dropped: DropCounts,
    since: Option<DateTime<Utc>>,
) -> Vec<String> {
    let mut totals = HitCounts::default();
    for s in sessions {
        totals.merge(&s.hit_counts);
    }
    let total_hits: usize = sessions
        .iter()
        .map(|s| s.hit_counts.at_or_above(min_tier))
        .sum();

    let tier_name = match min_tier {
        Tier::Prose => "prose",
        Tier::Tool => "prose+tool",
        Tier::Noise => "everything",
    };

    let window = match since {
        Some(cutoff) => format!(" since {}", format_timestamp(&cutoff)),
        None => String::new(),
    };
    let plural = |n: usize, word: &str| {
        if n == 1 {
            format!("{n} {word}")
        } else {
            format!("{n} {word}s")
        }
    };
    let mut lines = vec![format!(
        "{}, {} in {}{} ({})",
        plural(sessions.len(), "session"),
        plural(total_hits, "hit"),
        tier_name,
        window,
        totals.describe(min_tier),
    )];

    let (sort_name, other) = match sort {
        SortBy::Hits => ("most hits first", "--sort recency for newest match first"),
        SortBy::Recency => ("most recent match first", "--sort hits for densest first"),
    };

    // Name the session the other ordering would surface, so the tradeoff is
    // visible without re-running the search.
    let top_other = match sort {
        SortBy::Hits => sessions.iter().max_by_key(|s| s.last_hit),
        SortBy::Recency => sessions
            .iter()
            .max_by_key(|s| s.hit_counts.at_or_above(min_tier)),
    };
    let teaser = match top_other {
        Some(s) if sessions.len() > 1 => {
            let title = s.custom_title.as_deref().unwrap_or("(no title)");
            format!(
                " -> \"{}\" ({} hits)",
                truncate_clean(title, 48),
                s.hit_counts.at_or_above(min_tier)
            )
        }
        _ => String::new(),
    };
    lines.push(format!(
        "Showing {} of {}, {} | {}{}",
        show_count,
        sessions.len(),
        sort_name,
        other,
        teaser
    ));

    if dropped.total() > 0 {
        lines.push(widen_hint(min_tier, dropped));
    }

    let compacted = sessions.iter().filter(|s| s.compacted).count();
    if compacted > 0 {
        lines.push(format!("{compacted} of these sessions are compacted"));
    }

    lines
}

fn dirs_home() -> PathBuf {
    // HOME on macOS/Linux, USERPROFILE on Windows
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .expect("Could not determine home directory (neither HOME nor USERPROFILE is set)")
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tiers(v: serde_json::Value) -> Vec<(Tier, String)> {
        classify_entry(&v)
            .into_iter()
            .map(|f| (f.tier, f.label))
            .collect()
    }

    fn best(v: serde_json::Value, term: &str) -> Option<Tier> {
        classify_entry(&v)
            .into_iter()
            .filter(|f| fragment_matches(&f.text, &[term.to_string()], true))
            .map(|f| f.tier)
            .max()
    }

    #[test]
    fn strips_a_system_reminder_span() {
        assert_eq!(
            strip_system_reminders("before <system-reminder>hidden</system-reminder> after"),
            "before  after"
        );
    }

    #[test]
    fn strips_multiple_and_unterminated_reminders() {
        assert_eq!(
            strip_system_reminders("a<system-reminder>x</system-reminder>b<system-reminder>y"),
            "ab"
        );
    }

    #[test]
    fn leaves_text_without_reminders_alone() {
        assert_eq!(strip_system_reminders("plain text"), "plain text");
    }

    /// The bug: the injected skill listing mentions the term in every session.
    #[test]
    fn skill_listing_attachment_is_noise() {
        let entry = json!({
            "type": "attachment",
            "attachment": {
                "type": "skill_listing",
                "content": "- create-acme-invoice: Use when creating an invoice in Acme"
            }
        });
        assert_eq!(best(entry, "acme"), Some(Tier::Noise));
    }

    #[test]
    fn claude_md_instructions_attachment_is_noise() {
        let entry = json!({
            "type": "attachment",
            "attachment": {"type": "instructions", "files": [{"content": "Acme is our biller"}]}
        });
        assert_eq!(best(entry, "acme"), Some(Tier::Noise));
    }

    #[test]
    fn user_attached_file_counts_as_tool_tier() {
        let entry = json!({
            "type": "attachment",
            "attachment": {"type": "file", "content": "Acme report"}
        });
        assert_eq!(best(entry, "acme"), Some(Tier::Tool));
    }

    #[test]
    fn hook_and_history_entries_are_noise() {
        for t in ["system", "file-history-snapshot", "last-prompt", "queue-operation"] {
            let entry = json!({"type": t, "content": "Acme"});
            assert_eq!(best(entry, "acme"), Some(Tier::Noise), "type={t}");
        }
    }

    #[test]
    fn user_prose_is_prose_tier() {
        let entry = json!({
            "type": "user",
            "message": {"role": "user", "content": "let us invoice Acme today"}
        });
        assert_eq!(best(entry, "acme"), Some(Tier::Prose));
    }

    /// A term that appears only inside a system-reminder appended to a real
    /// user turn must not be promoted to prose.
    #[test]
    fn term_only_inside_system_reminder_is_noise() {
        let entry = json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "text", "text": "fix the build <system-reminder>skill: create-acme-invoice</system-reminder>"}
            ]}
        });
        assert_eq!(best(entry.clone(), "acme"), Some(Tier::Noise));
        assert_eq!(best(entry, "build"), Some(Tier::Prose));
    }

    #[test]
    fn tool_use_input_and_tool_result_are_tool_tier() {
        let call = json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "name": "Bash", "input": {"command": "ls acme-hub"}}
            ]}
        });
        assert_eq!(best(call, "acme"), Some(Tier::Tool));

        let result = json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "content": [{"type": "text", "text": "acme-hub-db-1"}]}
            ]}
        });
        assert_eq!(best(result, "acme"), Some(Tier::Tool));
    }

    #[test]
    fn assistant_thinking_is_prose_tier() {
        let entry = json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "Acme is the biller here"}
            ]}
        });
        assert_eq!(best(entry, "acme"), Some(Tier::Prose));
    }

    #[test]
    fn session_title_is_tool_tier() {
        let entry = json!({"type": "ai-title", "aiTitle": "Acme invoice run"});
        assert_eq!(best(entry, "acme"), Some(Tier::Tool));
    }

    /// A turn mixing prose and tool traffic yields both, so the session is
    /// ranked by its strongest evidence.
    #[test]
    fn mixed_turn_yields_both_tiers() {
        let entry = json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [
                {"type": "text", "text": "Checking Acme"},
                {"type": "tool_use", "name": "Bash", "input": {"command": "acme"}}
            ]}
        });
        let got = tiers(entry);
        assert!(got.contains(&(Tier::Prose, "assistant".to_string())));
        assert!(got.contains(&(Tier::Tool, "tool:Bash".to_string())));
    }

    #[test]
    fn all_terms_must_match_one_fragment() {
        let terms = vec!["acme".to_string(), "invoice".to_string()];
        assert!(fragment_matches("Acme invoice draft", &terms, true));
        assert!(!fragment_matches("Acme report", &terms, true));
        assert!(!fragment_matches("Acme invoice", &terms, false));
    }

    // --- --since parsing ---

    fn now() -> DateTime<Utc> {
        "2026-10-04T12:00:00Z".parse().expect("fixed instant")
    }

    #[test]
    fn parses_simple_durations() {
        for (input, secs) in [("90s", 90), ("30m", 1800), ("2h", 7200), ("7d", 604_800), ("3w", 1_814_400)] {
            assert_eq!(parse_duration(input), Some(secs), "input={input}");
        }
    }

    #[test]
    fn parses_compound_durations() {
        assert_eq!(parse_duration("1h30m"), Some(5400));
        assert_eq!(parse_duration("1d12h"), Some(129_600));
        assert_eq!(parse_duration("1w2d3h4m5s"), Some(788_645));
    }

    /// A partial parse would silently search the wrong window, so anything
    /// malformed must fail outright rather than parse the prefix it likes.
    #[test]
    fn rejects_malformed_durations() {
        for bad in ["", "30", "1h30", "h", "1x", "-5m", "m30", "1.5h", "1 h"] {
            assert_eq!(parse_duration(bad), None, "should reject {bad:?}");
        }
    }

    #[test]
    fn since_duration_is_relative_to_now() {
        assert_eq!(
            parse_since("2h", now()).unwrap(),
            "2026-10-04T10:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn since_accepts_rfc3339_timestamp() {
        assert_eq!(
            parse_since("2026-09-21T08:30:00Z", now()).unwrap(),
            "2026-09-21T08:30:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    /// A bare date means midnight on the user's calendar, not UTC's - so the
    /// parsed instant tracks the local offset rather than being 00:00Z.
    #[test]
    fn since_bare_date_is_local_midnight() {
        let parsed = parse_since("2026-09-21", now()).unwrap();
        let local_midnight = Local
            .from_local_datetime(
                &chrono::NaiveDate::from_ymd_opt(2026, 9, 21)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap(),
            )
            .earliest()
            .unwrap();
        assert_eq!(parsed, local_midnight.with_timezone(&Utc));
    }

    #[test]
    fn since_accepts_unix_seconds() {
        // 1760000000 = 2025-10-09T07:33:20Z
        assert_eq!(
            parse_since("1760000000", now()).unwrap(),
            DateTime::from_timestamp(1_760_000_000, 0).unwrap()
        );
    }

    /// Short all-digit input is a malformed duration, not a Unix timestamp -
    /// treating "30" as 1970 would silently match the whole archive.
    #[test]
    fn since_rejects_bare_number_without_unit() {
        assert!(parse_since("30", now()).is_err());
        assert!(parse_since("", now()).is_err());
        assert!(parse_since("banana", now()).is_err());
        assert!(parse_since("2026-13-45", now()).is_err());
    }

    // --- drop accounting ---

    /// The hint must not blame the tier for what the window excluded, since
    /// --tools cannot bring those sessions back.
    #[test]
    fn widen_hint_separates_window_from_tier() {
        let both = DropCounts { out_of_window: 685, below_tier: 8 };
        let msg = widen_hint(Tier::Prose, both);
        assert!(msg.contains("685 outside the --since window"), "{msg}");
        assert!(msg.contains("8 matching only in"), "{msg}");
        assert!(msg.contains("Hid 693"), "{msg}");

        let window_only = DropCounts { out_of_window: 5, below_tier: 0 };
        let msg = widen_hint(Tier::Prose, window_only);
        assert!(msg.contains("--since window"), "{msg}");
        assert!(!msg.contains("--tools"), "must not suggest --tools: {msg}");

        assert_eq!(widen_hint(Tier::Prose, DropCounts::default()), "");
    }

    #[test]
    fn hits_is_the_default_sort() {
        use clap::Parser;
        let cli = Cli::parse_from(["cc-search", "term"]);
        assert_eq!(cli.sort, SortBy::Hits);
        assert!(cli.since.is_none());
    }

    #[test]
    fn prose_is_the_default_tier() {
        assert_eq!(select_min_tier(false, false), Tier::Prose);
    }

    #[test]
    fn tools_flag_widens_to_tool_tier() {
        assert_eq!(select_min_tier(false, true), Tier::Tool);
    }

    #[test]
    fn all_flag_widens_to_noise_and_beats_tools() {
        assert_eq!(select_min_tier(true, false), Tier::Noise);
        assert_eq!(select_min_tier(true, true), Tier::Noise);
    }

    fn counts(user: usize, assistant: usize, tool: usize, noise: usize) -> HitCounts {
        HitCounts { user, assistant, tool, noise }
    }

    #[test]
    fn hit_counts_roll_up_by_tier() {
        let c = counts(2, 3, 4, 5);
        assert_eq!(c.at_or_above(Tier::Prose), 5);
        assert_eq!(c.at_or_above(Tier::Tool), 9);
        assert_eq!(c.at_or_above(Tier::Noise), 14);
    }

    /// The banner must not advertise hit kinds the active tier filtered out.
    #[test]
    fn describe_hides_kinds_below_the_active_tier() {
        let c = counts(2, 3, 4, 5);
        assert_eq!(c.describe(Tier::Prose), "2 user, 3 asst");
        assert_eq!(c.describe(Tier::Tool), "2 user, 3 asst, 4 tool");
        assert_eq!(c.describe(Tier::Noise), "2 user, 3 asst, 4 tool, 5 boilerplate");
    }

    #[test]
    fn describe_omits_kinds_with_no_hits() {
        assert_eq!(counts(0, 7, 0, 0).describe(Tier::Tool), "7 asst");
        assert_eq!(counts(0, 0, 0, 0).describe(Tier::Noise), "");
    }

    #[test]
    fn add_routes_each_fragment_to_its_kind() {
        let mut c = HitCounts::default();
        c.add(Tier::Prose, "user");
        c.add(Tier::Prose, "assistant");
        c.add(Tier::Tool, "tool:Bash");
        c.add(Tier::Noise, "skill_listing");
        assert_eq!((c.user, c.assistant, c.tool, c.noise), (1, 1, 1, 1));
    }

    #[test]
    fn merge_accumulates_across_sessions() {
        let mut total = counts(1, 2, 3, 4);
        total.merge(&counts(10, 20, 30, 40));
        assert_eq!((total.user, total.assistant, total.tool, total.noise), (11, 22, 33, 44));
    }

    #[test]
    fn widen_hint_names_the_flag_that_reveals_hidden_sessions() {
        let d = DropCounts { out_of_window: 0, below_tier: 5 };
        assert!(widen_hint(Tier::Prose, d).contains("--tools"));
        assert!(widen_hint(Tier::Tool, d).contains("--all"));
        // Nothing is hidden at the widest tier, so there is nothing to hint.
        assert_eq!(widen_hint(Tier::Noise, DropCounts::default()), "");
    }

    #[test]
    fn tier_ordering_is_noise_lt_tool_lt_prose() {
        assert!(Tier::Noise < Tier::Tool);
        assert!(Tier::Tool < Tier::Prose);
    }
}
