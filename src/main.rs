use chrono::{DateTime, Local, Utc};
use clap::Parser;
use colored::Colorize;
use rayon::prelude::*;
use serde::Deserialize;
use std::collections::HashMap;
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

    /// Custom Claude config directory
    #[arg(long)]
    claude_dir: Option<PathBuf>,
}

// --- Data model ---

#[derive(Deserialize)]
struct SessionPid {
    #[serde(rename = "sessionId")]
    session_id: String,
    cwd: Option<String>,
    #[serde(rename = "startedAt")]
    started_at: Option<u64>,
}

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

fn ripgrep_matching_lines(
    file: &Path,
    terms: &[String],
    ignore_case: bool,
    max_lines: usize,
) -> Vec<String> {
    if max_lines == 0 {
        return vec![];
    }

    let rg = find_rg();
    let mut cmd = Command::new(&rg);
    cmd.arg("--no-filename")
        .arg("--no-line-number")
        .arg("--no-messages");
    if ignore_case {
        cmd.arg("-i");
    }
    cmd.arg(&terms[0]).arg(file);

    let output = match cmd.output() {
        Ok(o) => o,
        Err(_) => return vec![],
    };

    let stdout = String::from_utf8_lossy(&output.stdout);

    stdout
        .lines()
        .filter(|line| {
            terms[1..].iter().all(|t| {
                if ignore_case {
                    line.to_lowercase().contains(&t.to_lowercase())
                } else {
                    line.contains(t.as_str())
                }
            })
        })
        .take(max_lines * 5)
        .map(String::from)
        .collect()
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
) -> Option<SessionInfo> {
    // Session files can be nested below the project directory (subagent logs
    // live in <project>/<session-uuid>/subagents/agent-*.jsonl), so the project
    // is the first path component under the projects directory — not the
    // file's immediate parent.
    let project = file
        .strip_prefix(projects_dir)
        .ok()
        .and_then(|rel| rel.components().next())
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .or_else(|| {
            Some(file.parent()?.file_name()?.to_string_lossy().to_string())
        })?;

    let session_id = file.file_stem()?.to_string_lossy().to_string();

    let content = std::fs::read_to_string(file).ok()?;

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

    for line in content.lines() {
        let entry: JsonlEntry = match serde_json::from_str(line) {
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

        if entry.entry_type.as_deref() == Some("custom-title") {
            if let Some(title) = &entry.custom_title {
                custom_title = Some(title.clone());
            }
        }

        if entry.entry_type.as_deref() == Some("ai-title") {
            if let Some(title) = &entry.ai_title {
                ai_title = Some(title.clone());
            }
        }

        // Extract cwd
        if cwd.is_none() {
            if let Ok(raw) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(c) = raw.get("cwd").and_then(|v| v.as_str()) {
                    cwd = Some(c.to_string());
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

    // Get matching lines - prefer user/assistant text, fall back to any entry
    let raw_matches = ripgrep_matching_lines(file, terms, ignore_case, context_lines);

    // First pass: only user/assistant messages with real text
    let mut matching_lines: Vec<MatchLine> = raw_matches
        .iter()
        .filter_map(|raw_line| {
            let entry: JsonlEntry = serde_json::from_str(raw_line).ok()?;
            let entry_type = entry.entry_type.as_deref()?;
            if entry_type != "user" && entry_type != "assistant" {
                return None;
            }
            let msg = entry.message.as_ref()?;
            let role = msg.role.as_deref().unwrap_or("system");
            let text = msg.content.as_ref().map(extract_text_content)?;
            if text.is_empty() || text.starts_with("[tool:") {
                return None;
            }
            if text.starts_with("<task-notification>") || text.starts_with("<system-reminder>") {
                return None;
            }

            let display = find_match_context(&text, terms, ignore_case, 150);

            Some(MatchLine {
                role: role.to_string(),
                text: display,
            })
        })
        .take(context_lines)
        .collect();

    // Fallback: if no user/assistant matches, search the raw JSONL for context
    if matching_lines.is_empty() {
        matching_lines = raw_matches
            .iter()
            .filter_map(|raw_line| {
                // Extract match context directly from the raw JSON line
                let display = find_match_context(raw_line, terms, ignore_case, 150);
                // Clean up JSON artifacts from the snippet
                let display = display
                    .replace("\\n", " ")
                    .replace("\\t", " ")
                    .replace("\\\"", "\"");
                if display.is_empty() {
                    return None;
                }
                let entry_type = serde_json::from_str::<JsonlEntry>(raw_line)
                    .ok()
                    .and_then(|e| e.entry_type)
                    .unwrap_or_else(|| "?".to_string());
                Some(MatchLine {
                    role: entry_type,
                    text: display,
                })
            })
            .take(context_lines)
            .collect();
    }

    Some(SessionInfo {
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

// --- PID session map ---

fn load_pid_sessions(claude_dir: &Path) -> HashMap<String, SessionPid> {
    let sessions_dir = claude_dir.join("sessions");
    let mut map = HashMap::new();

    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(e) => e,
        Err(_) => return map,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(pid_session) = serde_json::from_str::<SessionPid>(&content) {
                    map.insert(pid_session.session_id.clone(), pid_session);
                }
            }
        }
    }

    map
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

fn display_session(session: &SessionInfo, index: usize, _pid_map: &HashMap<String, SessionPid>) {
    let divider = "─".repeat(80);
    println!("{}", divider.dimmed());

    // Header
    let title = session.custom_title.as_deref().unwrap_or("(no title)");
    print!("{}  ", format!("#{}", index + 1).bold().cyan());
    println!("{}", title.bold().white());

    // Project + session ID
    let project_display = pretty_project(&session.project);
    println!(
        "  {}  {}",
        project_display.green(),
        session.session_id.dimmed()
    );

    // CWD if different from project
    if let Some(ref cwd) = session.cwd {
        println!("  {}", cwd.dimmed());
    }

    // Timestamps + duration
    if let (Some(first), Some(last)) = (session.first_timestamp, session.last_timestamp) {
        let duration = format_duration(&first, &last);
        println!(
            "  {} {} {} {}  {}",
            "from".dimmed(),
            format_timestamp(&first).yellow(),
            "to".dimmed(),
            format_timestamp(&last).yellow(),
            format!("({})", duration).dimmed()
        );
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

    println!(
        "  {} user / {} assistant msgs | {} entries | {}",
        session.user_msg_count.to_string().bold(),
        session.assistant_msg_count.to_string().bold(),
        session.total_entries,
        compact_str
    );

    // First user message
    if let Some(ref msg) = session.first_user_msg {
        let ts_str = msg
            .timestamp
            .as_ref()
            .map(format_timestamp)
            .unwrap_or_default();
        println!(
            "  {} {} {}",
            "FIRST:".blue().bold(),
            ts_str.dimmed(),
            msg.text
        );
    }

    // Last user message
    if let Some(ref msg) = session.last_user_msg {
        let ts_str = msg
            .timestamp
            .as_ref()
            .map(format_timestamp)
            .unwrap_or_default();
        println!(
            "  {}  {} {}",
            "LAST:".blue().bold(),
            ts_str.dimmed(),
            msg.text
        );
    }

    // Matching lines
    if !session.matching_lines.is_empty() {
        println!("  {}", "MATCHES:".magenta().bold());
        for m in &session.matching_lines {
            let role_tag = match m.role.as_str() {
                "user" => "user".cyan().to_string(),
                "assistant" => "asst".green().to_string(),
                "progress" => "tool".yellow().to_string(),
                "system" => "sys".dimmed().to_string(),
                other => other.dimmed().to_string(),
            };
            println!("    [{}] {}", role_tag, m.text);
        }
    }

    // Resume command
    let resume_cwd = session.cwd.as_deref().unwrap_or("~");
    if cfg!(target_os = "windows") {
        println!(
            "  {} cd /d {} & claude --resume {}",
            "RESUME:".dimmed(),
            resume_cwd,
            session.session_id.dimmed()
        );
    } else {
        println!(
            "  {} cd {} && claude --resume {}",
            "RESUME:".dimmed(),
            resume_cwd,
            session.session_id.dimmed()
        );
    }
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
    // e.g. home=/Users/ville → prefix="-Users-ville-"
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

    eprintln!(
        "{} Found {} matching sessions",
        ">>".blue().bold(),
        all_matches.len()
    );

    if all_matches.is_empty() {
        return;
    }

    // Load PID metadata
    let pid_map = load_pid_sessions(&claude_dir);

    // Parse in parallel
    let mut sessions: Vec<SessionInfo> = all_matches
        .par_iter()
        .filter_map(|f| parse_session(f, &projects_dir, &cli.terms, ignore_case, cli.context_lines))
        .collect();

    // Sort by last timestamp (most recent first)
    sessions.sort_by(|a, b| b.last_timestamp.cmp(&a.last_timestamp));

    // Display
    let show_count = sessions.len().min(cli.max_results);
    for (i, session) in sessions.iter().take(show_count).enumerate() {
        display_session(session, i, &pid_map);
    }

    let divider = "─".repeat(80);
    println!("{}", divider.dimmed());

    if sessions.len() > show_count {
        eprintln!(
            "{} Showing {} of {} results (use -n to show more)",
            ">>".blue().bold(),
            show_count,
            sessions.len()
        );
    }

    // Summary
    let compacted_count = sessions.iter().filter(|s| s.compacted).count();
    let total_msgs: usize = sessions.iter().map(|s| s.user_msg_count + s.assistant_msg_count).sum();
    eprintln!(
        "{} {} sessions, {} compacted, {} total messages",
        ">>".blue().bold(),
        sessions.len(),
        compacted_count,
        total_msgs
    );
}

fn dirs_home() -> PathBuf {
    // HOME on macOS/Linux, USERPROFILE on Windows
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .expect("Could not determine home directory (neither HOME nor USERPROFILE is set)")
}
