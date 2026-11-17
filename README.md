# AgentBox (`abox`)

A local flight recorder, safety guard, and step-by-step undo tool for AI coding agents (Claude Code MVP).

## Features Implemented (M0 + M1)
- **`abox init [--project | --global]` / `abox uninstall [--purge]`**: Detects Claude Code, backs up `settings.json`, non-destructively merges `SessionStart`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `Stop`, and `SessionEnd` hooks pointing directly to the native `abox` binary.
- **Shadow Git Snapshots**: Stores snapshots in `~/.agentbox/repos/<project-hash>` without touching your `.git`, index, or stash. Tracks **Bash / shell modifications** in addition to `Edit`/`Write`/`MultiEdit`, skips `node_modules`, `.aboxignore`, and files >20MB, and serializes concurrent writes via per-project file locks.
- **`abox log` / `abox show <step>` / `abox diff <step|a..b>`**: Unified timeline, file-level audit, and unified diffs.
- **`abox undo [<step>] [--to <step>] [--only <path>] [--force]`**: Restores files (including deleted files from Bash `rm -rf`), detects manual out-of-band modifications before overwriting, and creates a revertible undo snapshot before restoring.
- **High-Risk Guard (`PreToolUse`)**: Allows in-project operations covered by snapshots while prompting (`ask`) or blocking (`deny`) irreversible/sensitive operations (`git push --force`, `git reset --hard`, `rm -rf` outside project, `DROP TABLE`, `curl | sh`, `.env*`/`~/.ssh` writes), customizable via `~/.agentbox/policy.yaml` and `.agentbox/policy.yaml`.
- **`abox report` / `abox cost` / `abox gc` / `abox doctor` / `abox bench`**: Zero-hook historical Claude Code log audit card (`--json` / `--html`), branch/session/agent cost summary, GC, diagnostics, and 10k-file latency benchmark.

## Build & Verify
```bash
cargo build --release
cargo test
./target/release/abox bench --files 10000 --iters 20
```
