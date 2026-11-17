# Contributing to AgentBox

Thank you for contributing to AgentBox! This project is licensed under the [Apache License 2.0](LICENSE).

## Local Development

```bash
# Run linter (must pass with zero warnings)
cargo clippy --all-targets -- -D warnings

# Run unit & end-to-end integration tests
cargo test

# Build release binary
cargo build --release

# Run M0 10k-file snapshot latency benchmark
./target/release/abox bench --files 10000 --iters 20
```

## Architecture Notes
- **Fail-open hook execution**: `abox hook <agent>` must never block an AI coding agent on internal errors (log to `~/.agentbox/error.log` and exit `0`), except when a Guard rule intentionally returns `ask` or `deny`.
- **Zero-intrusion snapshots**: All Git operations run against `~/.agentbox/repos/<project-hash>` with an isolated `GIT_INDEX_FILE` so the user's `.git` directory, index, and stash remain untouched.
