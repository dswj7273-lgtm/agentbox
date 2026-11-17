# Changelog

## v0.1.0 (2026-11-17)

### Highlights
- **M0 Benchmark**: Verified shadow-git snapshot latency on 10,000-file repository (`p50 ~58ms`, `p95 ~59ms`, `p99 ~59ms`), beating the `p95 <= 80ms` target.
- **M1 Core (Claude Code MVP)**:
  - `abox init` / `abox uninstall` / `abox doctor` with non-destructive JSON hook merging and backups.
  - Shadow Git snapshot engine capturing Bash/shell edits alongside `Edit`/`Write`/`MultiEdit`, excluding `.gitignore`, `.aboxignore`, nested `.git` repos, and `>20MB` binaries.
  - `abox log`, `abox show`, `abox diff` (`<step>`, `<a>..<b>`, `cp:<tag>`), and safe `abox undo` (conflict detection, `--to`, `--only`, `--dry-run`, `--to-tag`, revertible undo snapshots).
  - High-risk Guard (`PreToolUse`) with compound shell splitting (`;`, `&&`, `||`, `|`, `sudo`, `env`, `bash -c`), `policy.yaml` overrides, and secret redaction.
  - Zero-hook `abox report` scanning local logs with streaming JSONL deduplication (`--json`, `--html`).
- **P1 Multi-Agent & Distribution**:
  - Added **Codex CLI** (`~/.codex/hooks.json`) and **Gemini CLI** (`BeforeTool`/`AfterTool`) adapters, plus unified cross-agent timeline and `abox cost --by branch|session|agent`.
  - Added `~/.agentbox/pricing.yaml` override support and `npm/` wrapper package (`npx agentbox`).
- **P2 Web UI, Watch, Policy & Team Export**:
  - `abox ui`: Zero-dependency local Web UI with timeline filtering, syntax-colored diffs, one-click step/file undo, and checkpoint creation/restoration.
  - `abox watch`: Filesystem watcher fallback for hookless editors like Cursor.
  - `abox export`: JSONL, OpenTelemetry (`OTLP/JSON`), and Markdown PR audit comment formats.
  - `abox policy` (`init`, `list`, `test`), `abox checkpoint`, `abox status`, `abox projects`, `abox log --follow`, `abox diff --stat`, `abox export --agent`, `abox doctor` SQLite `quick_check`, and `abox gc` storage-cap enforcement.
