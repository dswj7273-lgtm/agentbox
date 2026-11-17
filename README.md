# AgentBox (`abox`)

> **A flight recorder, safety net, and step-by-step undo button for AI coding agents — including Bash commands.**

[![CI](https://github.com/dswj7273-lgtm/agentbox/actions/workflows/ci.yml/badge.svg)](https://github.com/dswj7273-lgtm/agentbox/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)

Built-in agent checkpoints typically track only internal file-editing tools (`Edit`/`Write`) and miss changes made through shell commands (`rm`, `mv`, `sed`, codemods, build scripts). **AgentBox** takes lightweight filesystem snapshots into an isolated shadow Git repository—without touching your project's `.git`, index, or stash—and unifies **Claude Code**, **Codex CLI**, **Gemini CLI**, and hookless editors (**Cursor** via `abox watch`) onto a single auditable timeline.

---

## Key Features

- **Undo Bash & Shell Edits (`abox undo`)**: Restore files deleted or mutated by shell commands (`abox undo`, `abox undo --to <step>`, `abox undo <step> --only <path>`). Automatically detects manual out-of-band edits before overwriting and creates a revertible undo snapshot first.
- **Zero-Intrusion Shadow Git**: Stores snapshots in `~/.agentbox/repos/<project-hash>` with per-project file locks (`fs2`). Respects `.gitignore`, `.aboxignore`, skips `node_modules`/`target`/`dist` and files `>20MB`, and preserves nested `.git` repositories.
- **Cross-Agent Timeline**:
  - **Claude Code**: `SessionStart`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `Stop`, `SessionEnd`
  - **Codex CLI**: `SessionStart`, `PreToolUse`, `PostToolUse`, `Stop`, `SessionEnd`
  - **Gemini CLI**: `SessionStart`, `BeforeTool`, `AfterTool`, `SessionEnd`
  - **OpenCode**: `.opencode/plugins/agentbox.js` (`tool.execute.before`, `tool.execute.after`)
  - **Hookless Editors (Cursor, etc.)**: `abox watch --agent cursor` filesystem fallback
- **Minimal High-Risk Guard (`PreToolUse` / `BeforeTool`)**: Allows reversible in-project actions (covered by snapshots) while prompting (`ask`) or blocking (`deny`) irreversible/sensitive operations (`git push --force`, `git reset --hard`, `rm -rf` outside project, `DROP TABLE`, `curl | sh`, `.env*`/`~/.ssh` writes, or deleting `>20MB` excluded files).
- **Zero-Hook Audit Report (`abox report`)**: Scans local `~/.claude/projects`, `~/.codex/sessions`, and `~/.gemini/tmp` logs to summarize shell commands, high-risk commands, tokens, and estimated API-equivalent USD cost (`--json`, `--html`).
- **Local Web UI & Team Export**: `abox ui` launches a local timeline + diff + one-click undo web viewer; `abox export --format jsonl|otel|markdown` exports audit logs for OpenTelemetry or PR cost summaries.

---

## Quick Start

### Build from Source
```bash
cargo build --release
cargo install --path .
```

### Usage
```bash
# 1. Inspect your past 30 days immediately (no hooks needed)
abox report --days 30

# 2. Install hooks for detected agents & initialize shadow repo
abox init --project   # or `abox init --global`

# 3. Inspect timeline, step details, and diffs
abox log --all
abox show 12
abox diff 12

# 4. Undo safely (or preview with --dry-run, or use named checkpoints!)
abox checkpoint before-refactor
abox undo --to 12 --dry-run
abox undo --to 12
abox undo --to-tag before-refactor
abox undo 12 --only src/config.ts

# 5. Launch local Web UI, inspect status, or export audit trail
abox status
abox projects
abox ui
abox export --format otel --all --out audit-otel.json
```

See [examples/github-actions/agentbox-pr-comment.yml](examples/github-actions/agentbox-pr-comment.yml) for posting `abox export --format markdown` on Pull Requests.

---

## Configuration

### Custom Guard Rules (`.agentbox/policy.yaml` or `~/.agentbox/policy.yaml`)
Generate a starter template with `abox policy init`, inspect merged rules with `abox policy list`, or dry-run with `abox policy test Bash "git push --force"`.

```yaml
rules:
  - id: git.force_push
    match:
      tool: [Bash, run_shell_command]
      command: '\bgit\s+push\b.*(\s-f\b|\s--force\b)'
    action: ask
  - id: secrets.write
    match:
      tool: [Edit, Write, MultiEdit, write_file, edit_file]
      path: ["**/.env*", "**/.ssh/**"]
    action: deny
```

### Custom Model Pricing (`~/.agentbox/pricing.yaml`)
```yaml
models:
  claude-sonnet-4-5:
    input_per_million: 3.0
    output_per_million: 15.0
    cache_write_per_million: 3.75
    cache_read_per_million: 0.30
```

---

## Performance Benchmark

Measured on macOS APFS (`abox bench --files 10000 --iters 20`, release build):
- **p50**: `60.2 ms`
- **p95**: `61.5 ms` (target `<= 80 ms`)
- **p99**: `63.1 ms` (target `<= 200 ms`)

---

## License
Licensed under the [Apache License 2.0](LICENSE).
