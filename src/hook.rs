use crate::adapter::{get_adapter, HookEventKind, NormalizedHookEvent};
use crate::db::{Database, NewStepInput};
use crate::guard::{redact_sensitive_text, Decision, GuardEngine};
use crate::paths::{detect_git_branch, project_hash, resolve_project_root, AgentBoxPaths};
use crate::snapshot::{
    capture_snapshot, capture_snapshot_with_mode, diff_trees, is_mutating_tool, ProjectLock,
    SnapshotMode,
};
use anyhow::{bail, Context, Result};
use std::io::{Read, Write};

pub fn run_hook(agent_name: &str) -> Result<()> {
    let mut raw_input = String::new();
    std::io::stdin()
        .read_to_string(&mut raw_input)
        .context("Failed to read hook payload from stdin")?;

    if raw_input.trim().is_empty() {
        return Ok(());
    }

    let paths = AgentBoxPaths::resolve()?;
    match handle_hook_inner(agent_name, &raw_input, &paths) {
        Ok(Some(stdout_json)) => {
            println!("{}", stdout_json);
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(err) => {
            // Fail-open guarantee (PRD §8.2 & §9): never block the agent when AgentBox itself errors.
            log_hook_error(&paths, agent_name, &err);
            Ok(())
        }
    }
}

pub fn handle_hook_inner(
    agent_name: &str,
    raw_input: &str,
    paths: &AgentBoxPaths,
) -> Result<Option<String>> {
    let Some(adapter) = get_adapter(agent_name) else {
        bail!("Unknown agent adapter: {}", agent_name);
    };
    let event = adapter.parse_hook_event(raw_input)?;
    paths.ensure_dirs()?;

    let project_root = resolve_project_root(&event.cwd);
    let proj_hash = project_hash(&project_root);
    let shadow_repo = paths.shadow_repo_dir(&proj_hash);
    let lock_path = paths.lock_path(&proj_hash);
    let mut db = Database::open(&paths.db_path())?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let now_s = now_ms / 1000;

    let branch = detect_git_branch(&project_root);
    db.ensure_session(
        &event.session_id,
        &event.agent,
        &project_root.to_string_lossy(),
        &proj_hash,
        branch.as_deref(),
        now_s,
    )?;

    match event.kind {
        HookEventKind::SessionStart => {
            let _lock = ProjectLock::acquire(&lock_path)?;
            let prev_tree = db.get_project_last_tree(&proj_hash)?;
            let snap = capture_snapshot(
                &project_root,
                &shadow_repo,
                Some(&event.session_id),
                prev_tree.as_deref(),
            )?;
            db.upsert_project_baseline(
                &proj_hash,
                &project_root.to_string_lossy(),
                Some(&snap.tree_hash),
                Some(&snap.tree_hash),
                now_s,
            )?;
            Ok(None)
        }
        HookEventKind::PreToolUse => {
            let tool = event.tool_name.clone().unwrap_or_else(|| "Unknown".into());
            let global_policy = paths.global_policy_path();
            let project_policy = project_root.join(".agentbox").join("policy.yaml");
            let guard = GuardEngine::load(
                global_policy.exists().then_some(global_policy.as_path()),
                project_policy.exists().then_some(project_policy.as_path()),
            );

            let mutating = is_mutating_tool(&tool);
            let skipped_large = if mutating && shadow_repo.exists() {
                read_excluded_large_files(&shadow_repo)
            } else {
                Vec::new()
            };

            let verdict = guard.evaluate(&tool, &event.tool_input, &project_root, &skipped_large);
            let summary = summarize_tool_input(&tool, &event.tool_input);
            let redacted_json =
                redact_sensitive_text(&serde_json::to_string(&event.tool_input).unwrap_or_default());

            if verdict.decision == Decision::Deny {
                // Record denied step immediately; PostToolUse won't fire for denied calls
                let input = NewStepInput {
                    session_id: event.session_id.clone(),
                    agent: event.agent.clone(),
                    tool: tool.clone(),
                    input_summary: summary,
                    input_json: redacted_json,
                    pre_tree: None,
                    post_tree: None,
                    files_changed: 0,
                    guard_decision: Some("deny".to_string()),
                    guard_rule: verdict.rule_id.clone(),
                    exit_status: None,
                    duration_ms: Some(0),
                    started_at: now_s,
                    tokens: 0,
                    cost_usd: 0.0,
                    is_undo: false,
                    concurrent_overlap: false,
                    tool_use_id: event.tool_use_id.clone(),
                };
                db.insert_step(&input, &[])?;

                let reason = format_guard_message(
                    verdict.rule_id.as_deref(),
                    verdict.reason.as_deref().unwrap_or("Blocked by AgentBox policy"),
                );
                return Ok(adapter.format_pretool_decision("deny", &reason));
            }

            let pre_tree = if mutating {
                let _lock = ProjectLock::acquire(&lock_path)?;
                let prev_tree = db.get_project_last_tree(&proj_hash)?;
                let snap = capture_snapshot_with_mode(
                    &project_root,
                    &shadow_repo,
                    Some(&event.session_id),
                    prev_tree.as_deref(),
                    SnapshotMode::FastIncremental,
                )?;
                db.upsert_project_baseline(
                    &proj_hash,
                    &project_root.to_string_lossy(),
                    Some(&snap.tree_hash),
                    Some(&snap.tree_hash),
                    now_s,
                )?;
                Some(snap.tree_hash)
            } else {
                None
            };

            let tid = event
                .tool_use_id
                .clone()
                .unwrap_or_else(|| format!("{}-{}-{}", event.session_id, tool, now_ms));
            let _ = db.record_inflight_start(
                &tid,
                &event.session_id,
                &proj_hash,
                &event.agent,
                &tool,
                &summary,
                &redacted_json,
                pre_tree.as_deref(),
                Some(verdict.decision.as_str()),
                verdict.rule_id.as_deref(),
                now_ms,
            )?;

            if verdict.decision == Decision::Ask {
                let reason = format_guard_message(
                    verdict.rule_id.as_deref(),
                    verdict
                        .reason
                        .as_deref()
                        .unwrap_or("Confirmation required by AgentBox policy"),
                );
                return Ok(adapter.format_pretool_decision("ask", &reason));
            }

            Ok(None)
        }
        HookEventKind::PostToolUse | HookEventKind::PostToolUseFailure => {
            let tool = event.tool_name.clone().unwrap_or_else(|| "Unknown".into());
            let (inflight, concurrent_overlap) =
                db.take_inflight(event.tool_use_id.as_deref(), &event.session_id, &tool)?;

            let mutating = is_mutating_tool(&tool);
            let pre_tree = inflight
                .as_ref()
                .and_then(|i| i.pre_tree.clone())
                .or_else(|| db.get_project_last_tree(&proj_hash).ok().flatten());

            let (post_tree, file_changes) = if mutating {
                let _lock = ProjectLock::acquire(&lock_path)?;
                let snap = capture_snapshot_with_mode(
                    &project_root,
                    &shadow_repo,
                    Some(&event.session_id),
                    pre_tree.as_deref(),
                    SnapshotMode::FastIncremental,
                )?;
                let changes = if let Some(ref pre) = pre_tree {
                    diff_trees(&shadow_repo, pre, &snap.tree_hash, true).unwrap_or_default()
                } else {
                    Vec::new()
                };
                (Some(snap.tree_hash), changes)
            } else {
                (None, Vec::new())
            };

            let summary = inflight
                .as_ref()
                .map(|i| i.input_summary.clone())
                .unwrap_or_else(|| summarize_tool_input(&tool, &event.tool_input));
            let input_json = inflight
                .as_ref()
                .map(|i| i.input_json.clone())
                .unwrap_or_else(|| {
                    redact_sensitive_text(
                        &serde_json::to_string(&event.tool_input).unwrap_or_default(),
                    )
                });
            let duration_ms = inflight
                .as_ref()
                .map(|i| (now_ms - i.started_at).max(0));

            let step_input = NewStepInput {
                session_id: event.session_id.clone(),
                agent: event.agent.clone(),
                tool,
                input_summary: summary,
                input_json,
                pre_tree,
                post_tree: post_tree.clone(),
                files_changed: file_changes.len() as i64,
                guard_decision: inflight.as_ref().and_then(|i| i.guard_decision.clone()),
                guard_rule: inflight.as_ref().and_then(|i| i.guard_rule.clone()),
                exit_status: event.exit_status,
                duration_ms,
                started_at: inflight
                    .as_ref()
                    .map(|i| i.started_at / 1000)
                    .unwrap_or(now_s),
                tokens: 0,
                cost_usd: 0.0,
                is_undo: false,
                concurrent_overlap,
                tool_use_id: event.tool_use_id.clone(),
            };

            let step_id = db.insert_step(&step_input, &file_changes)?;
            if let Some(ref pt) = post_tree {
                db.update_project_last_tree(&proj_hash, pt, Some(step_id), now_s)?;
            }
            Ok(None)
        }
        HookEventKind::Stop | HookEventKind::SessionEnd => {
            enrich_session_from_transcript(&mut db, &event, now_s)?;
            Ok(None)
        }
    }
}

fn enrich_session_from_transcript(
    db: &mut Database,
    event: &NormalizedHookEvent,
    now_s: i64,
) -> Result<()> {
    if let Some(ref tp) = event.transcript_path {
        if tp.exists() {
            let parsed = match event.agent.as_str() {
                "claude" => crate::adapter::claude::parse_claude_transcript(tp).ok(),
                "codex" => crate::adapter::codex::parse_codex_transcript(tp).ok(),
                "gemini" => crate::adapter::gemini::parse_gemini_transcript(tp).ok(),
                _ => None,
            };
            if let Some(metrics) = parsed {
                let cache_total = metrics.cache_write_tokens + metrics.cache_read_tokens;
                db.finish_session(
                    &event.session_id,
                    now_s,
                    metrics.input_tokens,
                    metrics.output_tokens,
                    cache_total,
                    metrics.cost_usd,
                )?;
                let _ = db.update_step_costs_by_tool_use_id(&event.session_id, &metrics.tool_costs);
                return Ok(());
            }
        }
    }
    db.finish_session(&event.session_id, now_s, 0, 0, 0, 0.0)?;
    Ok(())
}

pub fn summarize_tool_input(tool: &str, input: &serde_json::Value) -> String {
    match tool {
        "Bash" | "run_shell_command" | "shell_command" | "exec_command" => input
            .get("command")
            .or_else(|| input.get("cmd"))
            .and_then(|v| v.as_str())
            .map(|s| truncate_one_line(s, 72))
            .unwrap_or_else(|| "(command)".into()),
        "Edit" | "Write" | "MultiEdit" | "Read" | "NotebookEdit" | "write_file" | "edit_file" => {
            input
                .get("file_path")
                .or_else(|| input.get("path"))
                .or_else(|| input.get("notebook_path"))
                .and_then(|v| v.as_str())
                .map(|s| truncate_path_tail(s, 60))
                .unwrap_or_else(|| "(file)".into())
        }
        _ => {
            if let Some(p) = input.get("pattern").and_then(|v| v.as_str()) {
                truncate_one_line(p, 60)
            } else {
                truncate_one_line(&input.to_string(), 60)
            }
        }
    }
}

fn truncate_one_line(s: &str, max_chars: usize) -> String {
    let single = s.replace(['\n', '\r'], " ");
    let trimmed = single.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        let head: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{}…", head)
    }
}

fn truncate_path_tail(p: &str, max_chars: usize) -> String {
    if p.chars().count() <= max_chars {
        p.to_string()
    } else {
        let len = p.chars().count();
        let tail: String = p.chars().skip(len - (max_chars - 1)).collect();
        format!("…{}", tail)
    }
}

fn format_guard_message(rule_id: Option<&str>, reason: &str) -> String {
    format!(
        "AgentBox [{}]: {} (Override in .agentbox/policy.yaml)",
        rule_id.unwrap_or("policy"),
        reason
    )
}

fn log_hook_error(paths: &AgentBoxPaths, agent: &str, err: &anyhow::Error) {
    let _ = paths.ensure_dirs();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.error_log_path())
    {
        let ts = chrono::Utc::now().to_rfc3339();
        let _ = writeln!(f, "[{}] hook={} error={:#}", ts, agent, err);
    }
}

fn read_excluded_large_files(shadow_repo: &std::path::Path) -> Vec<String> {
    let exclude_path = shadow_repo.join("info").join("exclude");
    let Ok(raw) = std::fs::read_to_string(exclude_path) else {
        return Vec::new();
    };
    let mut in_block = false;
    let mut out = Vec::new();
    for line in raw.lines() {
        if line.contains("agentbox auto-excluded >20MB") {
            in_block = true;
            continue;
        }
        if line.contains("end agentbox auto-excluded") {
            break;
        }
        if in_block {
            let trimmed = line.trim().trim_start_matches('/');
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
    }
    out
}
