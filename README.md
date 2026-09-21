# cc-search

A fast full-text search tool for [Claude Code](https://docs.anthropic.com/en/docs/claude-code) conversation history. Finds sessions by keyword, identifies each session's state (compaction, message counts, timestamps), and prints a ready-to-use `claude --resume` command.

> *This is an independent community tool. Not affiliated with or endorsed by Anthropic.*

## Why

Claude Code stores conversation history as JSONL files spread across `~/.claude/projects/`. After weeks of use this grows to hundreds of sessions and gigabytes of data. Finding "that session where I worked on X" means grepping through thousands of files and then manually piecing together which session it was, when it happened, and whether context was compacted away.

`cc-search` solves this in one command.

## Quick Start

```bash
# Build
cargo build --release

# Search all sessions for a keyword
cc-search "authentication"

# AND search: all terms must appear in the session
cc-search "intercom" "article"

# Restrict to a project
cc-search "pricing" -p myproject

# Show more results with more context lines
cc-search "bug" -n 50 -c 5
```

## Installation

### Prerequisites

- **Rust** (edition 2024) — install via [rustup](https://rustup.rs/)
- **ripgrep** (`rg`) — install via `brew install ripgrep` or your package manager

### Build & Install

```bash
cd /path/to/cc-search
cargo build --release

# Option A: copy to a directory in your PATH
cp target/release/cc-search ~/.local/bin/

# Option B: symlink
ln -s "$(pwd)/target/release/cc-search" ~/.local/bin/cc-search
```

Binary size: ~1.1 MB (with LTO).

## Usage

```
cc-search [OPTIONS] <TERMS>...
```

### Arguments

| Argument | Description |
|----------|-------------|
| `<TERMS>...` | One or more search terms. **All terms must match** within the same session file (AND logic). |

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `-s`, `--case-sensitive` | off | Enable case-sensitive matching. Default is case-insensitive. |
| `-n`, `--max-results` | 20 | Maximum number of sessions to display. |
| `-c`, `--context-lines` | 3 | Number of matching message excerpts to show per session. |
| `-p`, `--project` | all | Filter to sessions from a specific project (substring match on project directory name). |
| `--include-agents` | off | Include subagent/agent sessions (excluded by default). |
| `--sort` | `recency` | Result ordering: `recency` (most recently active first) or `hits` (most matches first). |
| `--tools` | off | Also match tool calls, tool output and session titles, not just prose. |
| `--all` | off | Match anything in the transcript, including auto-injected boilerplate. |
| `--claude-dir` | `~/.claude` | Override the Claude configuration directory. |

#### Match tiers

A session `.jsonl` contains far more than the conversation. Claude Code injects the
skill listing, `CLAUDE.md`, hook output, token reminders and file-history snapshots
into *every* transcript, so a raw substring hit says nothing about whether the
session was actually *about* the term. On one real archive, searching for a
customer name matched 560 sessions — 312 of them only because an unrelated
`create-acme-invoice` skill description mentioned the word.

Every transcript entry is therefore classified into a tier, and a session is only
reported if it has a hit at or above the requested tier:

| Tier | Contains | Included by |
|------|----------|-------------|
| **prose** | User text, assistant text, assistant thinking (with `<system-reminder>` spans stripped) | default, `--tools`, `--all` |
| **tool** | Tool call inputs, tool output, user-attached files, session titles | `--tools`, `--all` |
| **noise** | Skill listings, `CLAUDE.md`, system reminders, hook output, token reminders, file-history snapshots, queue operations | `--all` |

**Prose is the default**, because the question a history search answers is
"where did we talk about this?" — and tool output is full of incidental
mentions, where an `ls` of a directory whose name contains the term matches
every time. Widen deliberately: `--tools` for "which session *did* this work?",
`--all` for a literal file search. When sessions are hidden, the count and the
flag that would reveal them are reported on stderr.

### Examples

```bash
# Find sessions mentioning "migration" in the odoo project
cc-search "migration" -p odoo

# Case-sensitive search for an exact function name
cc-search -s "computePriceUnit"

# Show all matches with 10 context lines each
cc-search "deploy" -c 10

# Include subagent sessions (normally filtered out)
cc-search "test failure" --include-agents

# Find where the work on a topic actually happened, not what mentioned it last
cc-search "Acme" --sort hits

# Widen past prose to tool calls and tool output
cc-search "Acme" --tools

# Literal file search, boilerplate and all (the pre-tier behaviour)
cc-search "Acme" --all

# Show the top 5 most recent sessions mentioning two terms
cc-search "docker" "compose" -n 5
```

### Reading the banner

The `>>` banner is written to **stderr**, before *and* after the results:

```
>> Searching for: Acme
>> 111 sessions, 413 hits in prose (134 user, 279 asst)
>> Showing 20 of 111, most recent first | --sort hits for densest first -> "Sales invoice report from Acme" (22 hits)
>> Hid 451 session(s) matching only in tool calls, tool output or boilerplate (--tools / --all to widen)
```

Both placements are deliberate. `cc-search "Acme" | head -40` truncates stdout
but not stderr, so the totals survive the pipe — and the leading copy means you
see them even when `head` cuts the run short. Each line names the flag that
changes it, and the `->` teaser names the session the *other* ordering would put
first, so you can tell whether re-sorting is worth it without re-running.

Per-session counts appear on the `MATCHES:` line:

```
MATCHES: [22 hits (1 user, 21 asst) - showing 3]
```

Hit kinds below the active tier are not listed, since they did not qualify the
session: at the default you see `user`/`asst`, with `--tools` also `tool`, with
`--all` also `boilerplate`.

## Output Format

Each matching session is displayed as a card:

```
────────────────────────────────────────────────────────────────────────────────
#1  My Session Title
  my-project  8fd699c4-5f9b-49cd-90c1-e5270f2282c0
  /home/user/code/my-project
  from 2026-03-08 11:30 to 2026-03-21 01:07  (12d 13h)
  177 user / 246 assistant msgs | 1098 entries | not compacted
  FIRST: 2026-03-08 11:41 the first user message in this session...
  LAST:  2026-03-21 01:05 the last user message in this session...
  MATCHES:
    [user] ...context around the matching keyword...
    [asst] ...found the integration that fetches...
  RESUME: cd /home/user/code/my-project && claude --resume 8fd699c4-...
```

### Fields Explained

| Field | Description |
|-------|-------------|
| **#N + Title** | Result rank and session title: the custom title if set via Claude Code, otherwise the auto-generated AI title. |
| **Project** | Human-readable project name, derived from the directory path. |
| **Session ID** | UUID used with `claude --resume`. |
| **CWD** | Working directory the session was started in. |
| **Timestamps** | First and last entry timestamps (local time) with total duration. |
| **Message counts** | Number of user messages, assistant messages, and total JSONL entries. |
| **Compaction** | Whether Claude compacted (summarized) the conversation to free context. Shows count if compacted multiple times (e.g., `COMPACTED (4x)`). |
| **FIRST** | First real user message (skips compaction summaries). |
| **LAST** | Last user message in the session. |
| **MATCHES** | Excerpts from messages where the search terms were found, with `[user]`/`[asst]` role tags. Centered on the match location with surrounding context. |
| **RESUME** | Copy-pasteable command to resume the session in Claude Code. |

### Summary Line

After all results, a summary is printed:

```
>> 47 sessions, 10 compacted, 15182 total messages
```

## Architecture

### Search Strategy

The tool uses a two-phase approach optimized for speed:

1. **Phase 1 — ripgrep file discovery**: For each search term, `rg --files-with-matches` scans all `.jsonl` files. Multiple terms are intersected sequentially (the result set shrinks with each term, so subsequent scans are faster).

2. **Phase 2 — parallel Rust parsing and tier filtering**: Only the matched files are read and parsed. [Rayon](https://docs.rs/rayon) parallelizes this across CPU cores. Each session file is parsed once to extract metadata, timestamps, message counts, compaction status, and matching excerpts. Ripgrep's file list is a *candidate* set: a candidate is dropped here if its only hit is below the requested match tier, which is what keeps injected boilerplate out of the results.

### Why ripgrep + Rust (not pure Rust)?

Raw text search through ~1 GB of JSONL is ripgrep's sweet spot — it uses memory-mapped I/O, SIMD, and optimized regex matching. The structured parsing (JSON deserialization, timestamp comparison, output formatting) is where Rust adds value. The combination is faster than either approach alone.

### Data Layout

Claude Code stores conversation data in `~/.claude/`:

```
~/.claude/
├── sessions/
│   ├── <pid>.json                      # PID → session metadata (sessionId, cwd, startedAt)
│   └── ...
└── projects/
    ├── -Users-<user>-code-<project>/
    │   ├── <session-uuid>.jsonl        # Conversation log (one JSON object per line)
    │   ├── <session-uuid>/
    │   │   └── subagents/
    │   │       └── agent-<hash>.jsonl  # Subagent conversation logs
    │   └── memory/                     # Project memory files
    └── ...
```

Each `.jsonl` line is a JSON object with fields:

| Field | Description |
|-------|-------------|
| `type` | Entry type: `user`, `assistant`, `progress`, `system`, `file-history-snapshot`, `custom-title`, etc. |
| `message.role` | `user` or `assistant` (only on message entries). |
| `message.content` | String or array of content blocks (text, tool_use, tool_result, thinking). |
| `timestamp` | ISO 8601 UTC timestamp. |
| `sessionId` | UUID linking to the session. |
| `cwd` | Working directory at time of entry. |

### Compaction Detection

When Claude Code runs out of context window, it compacts the conversation by replacing earlier messages with a summary. This is detected by looking for user messages containing:

> "This session is being continued from a previous conversation that ran out of context."

Sessions may be compacted multiple times in long-running conversations (tracked as `compaction_count`).

### Noise Filtering

The tool automatically filters out:

- **Agent sessions** (`agent-*.jsonl`) unless `--include-agents` is passed
- **Noise-tier entries** (see [Match tiers](#match-tiers)) unless `--all` is passed — this is the main defence against false matches
- **`<system-reminder>` spans** anywhere inside a user or assistant turn, not just at the start: the harness appends them to real messages, and they routinely carry the entire skill listing
- **Task notifications** (`<task-notification>` prefixed messages)
- **Compaction summaries** from first/last message display (the original user messages are shown instead)

Matching excerpts are sorted strongest-evidence-first, so a prose hit is shown ahead of a tool-output hit from the same session.

## Performance

Benchmarked on a real dataset: **2,338 sessions, 942 MB** of JSONL data on Apple Silicon (M-series).

| Query | Sessions matched | Wall time |
|-------|-----------------|-----------|
| Single broad term (316 hits) | 316 | **0.13s** |
| Two-term AND search (13 hits) | 13 | **0.21s** |
| Single term + project filter (26 hits) | 26 | **0.15s** |

Performance scales linearly with data size. The bottleneck is ripgrep's initial scan; Rust parsing of matched files is negligible thanks to Rayon parallelization.

## Dependencies

| Crate | Purpose |
|-------|---------|
| [clap](https://docs.rs/clap) | CLI argument parsing with derive macros |
| [serde](https://docs.rs/serde) + [serde_json](https://docs.rs/serde_json) | JSON deserialization of JSONL entries |
| [chrono](https://docs.rs/chrono) | Timestamp parsing (ISO 8601) and local time display |
| [colored](https://docs.rs/colored) | Terminal color output |
| [rayon](https://docs.rs/rayon) | Parallel iteration for session parsing |

External: [ripgrep](https://github.com/BurntSushi/ripgrep) (`rg`) — must be installed separately.

## Platform Support

| Platform | Status | Notes |
|----------|--------|-------|
| **macOS** (Intel & Apple Silicon) | Tested | Primary development platform. ripgrep detected via Homebrew paths. |
| **Linux** | Expected to work | ripgrep must be in `PATH` or standard locations (`/usr/bin/rg`, `/usr/local/bin/rg`). |
| **Windows** | Expected to work | Uses `USERPROFILE` env var for home directory. ripgrep detected via Chocolatey path or `PATH`. Resume command uses `cd /d` syntax. |

The tool dynamically detects the home directory (`HOME` on Unix, `USERPROFILE` on Windows) and derives project names from it, so project name display works across all platforms without configuration.

## Limitations

- **ripgrep required**: The tool shells out to `rg` for the initial search phase. If `rg` is not installed, it will exit with an error. Install via `brew install ripgrep` (macOS), `apt install ripgrep` (Debian/Ubuntu), `choco install ripgrep` (Windows), or see [ripgrep installation](https://github.com/BurntSushi/ripgrep#installation).
- **No regex in search terms**: Search terms are passed as literal strings to ripgrep. However, since they go directly to `rg`, you can use ripgrep's regex syntax if needed (e.g., `cc-search "deploy(ed|ing)"`).
- **AND logic only**: All terms must appear somewhere in the session file. There is no OR or phrase matching — each term is matched independently.

## License

MIT
