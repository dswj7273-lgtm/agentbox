use crate::adapter::{all_adapters, InstallScope};
use crate::db::{Database, LogFilter, NewStepInput};
use crate::paths::{
    current_exe_path, detect_git_branch, project_hash, resolve_project_root, AgentBoxPaths,
};
use crate::report::format_tokens;
use crate::snapshot::{
    capture_snapshot, capture_snapshot_with_mode, detect_external_modifications, diff_patch,
    diff_trees, dir_size_bytes, ensure_shadow_repo, restore_to_tree, run_git_gc, ProjectLock,
    SnapshotMode,
};
use anyhow::{Context, Result};
use chrono::{Local, TimeZone};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Instant;

pub fn cmd_init(project_scope: bool, global_scope: bool) -> Result<()> {
    let paths = AgentBoxPaths::resolve()?;
    paths.ensure_dirs()?;
    let _ = Database::open(&paths.db_path())?;

    let cwd = std::env::current_dir()?;
    let project_root = resolve_project_root(&cwd);
    let proj_hash = project_hash(&project_root);
    let shadow_repo = paths.shadow_repo_dir(&proj_hash);
    let abox_bin = current_exe_path()?;

    let scope = if project_scope && !global_scope {
        InstallScope::Project
    } else {
        InstallScope::Global
    };

    let adapters = all_adapters();
    let mut detected = Vec::new();
    let mut installed_any = false;

    for adapter in &adapters {
        if let Some(ver) = adapter.detect() {
            detected.push(format!("{} ({})", adapter.name(), ver));
            let report = adapter.install_hooks(scope, &project_root, &abox_bin)?;
            installed_any = true;
            println!("✔ Detected: {}", detected.last().unwrap());
            if let Some(bak) = report.backup_path {
                println!(
                    "✔ Hooks installed → {} (backup: {})",
                    report.config_path.display(),
                    bak.display()
                );
            } else {
                println!("✔ Hooks installed → {}", report.config_path.display());
            }
            if let Some(note) = report.post_install_note {
                println!("  ℹ {}", note);
            }
        }
    }

    if !installed_any {
        println!("⚠ No supported agent CLI detected (~/.claude, ~/.codex, ~/.gemini not found).");
        println!("  Creating Claude Code hooks anyway at target scope...");
        if let Some(claude) = adapters.first() {
            let report = claude.install_hooks(scope, &project_root, &abox_bin)?;
            println!("✔ Hooks installed → {}", report.config_path.display());
        }
    }

    let lock_path = paths.lock_path(&proj_hash);
    let _lock = ProjectLock::acquire(&lock_path)?;
    ensure_shadow_repo(&project_root, &shadow_repo)?;
    let snap = capture_snapshot(&project_root, &shadow_repo, None, None)?;
    let mut db = Database::open(&paths.db_path())?;
    let now_s = chrono::Utc::now().timestamp();
    db.upsert_project_baseline(
        &proj_hash,
        &project_root.to_string_lossy(),
        Some(&snap.tree_hash),
        Some(&snap.tree_hash),
        now_s,
    )?;
    // Touch db mut ref cleanly
    let _ = &mut db;

    println!(
        "✔ Shadow repo ready for {} (tree {})",
        project_root.display(),
        &snap.tree_hash[..8]
    );
    println!("→ Try: run your agent, then `abox log`");
    Ok(())
}

pub fn cmd_uninstall(project_scope: bool, global_scope: bool, purge: bool) -> Result<()> {
    let paths = AgentBoxPaths::resolve()?;
    let cwd = std::env::current_dir()?;
    let project_root = resolve_project_root(&cwd);

    let scopes = if project_scope && !global_scope {
        vec![InstallScope::Project]
    } else if global_scope && !project_scope {
        vec![InstallScope::Global]
    } else {
        vec![InstallScope::Global, InstallScope::Project]
    };

    for adapter in all_adapters() {
        for &scope in &scopes {
            if let Some(cfg) = adapter.uninstall_hooks(scope, &project_root)? {
                println!("✔ Removed AgentBox hooks from {}", cfg.display());
            }
        }
    }

    if purge {
        if paths.home_dir.exists() {
            std::fs::remove_dir_all(&paths.home_dir)?;
            println!("✔ Purged AgentBox data directory: {}", paths.home_dir.display());
        }
    }
    Ok(())
}

pub fn cmd_log(
    all_sessions: bool,
    agent: Option<String>,
    since: Option<String>,
    file: Option<String>,
    limit: usize,
) -> Result<()> {
    let paths = AgentBoxPaths::resolve()?;
    if !paths.db_path().exists() {
        println!("No AgentBox history yet. Run `abox init` first.");
        return Ok(());
    }
    let db = Database::open(&paths.db_path())?;
    let cwd = std::env::current_dir()?;
    let project_root = resolve_project_root(&cwd);
    let project_str = project_root.to_string_lossy().to_string();

    let since_ts = since.as_deref().and_then(parse_since_duration);

    let filter = LogFilter {
        project: Some(project_str.clone()),
        all_sessions,
        agent,
        since_ts,
        file_path: file,
        limit,
    };

    let steps = db.query_steps(&filter)?;
    if steps.is_empty() {
        println!("No recorded steps for {}.", project_root.display());
        return Ok(());
    }

    println!(
        " {:<4} {:<8} {:<7} {:<8} {:<36} {:>5}  {:<8}",
        "#", "TIME", "AGENT", "TOOL", "SUMMARY", "FILES", "STATUS"
    );

    let mut total_files = 0i64;
    for s in &steps {
        let time_str = Local
            .timestamp_opt(s.started_at, 0)
            .single()
            .map(|dt| dt.format("%H:%M:%S").to_string())
            .unwrap_or_else(|| "--:--:--".to_string());

        let mut summary = s.input_summary.clone();
        if s.concurrent_overlap {
            summary.push_str(" [concurrent]");
        }
        let summary_col = pad_or_truncate(&summary, 36);

        let status = match s.guard_decision.as_deref() {
            Some("deny") => "DENIED".to_string(),
            Some("ask") => "ASKED".to_string(),
            _ => match s.exit_status {
                Some(0) | None => {
                    if s.is_undo {
                        "UNDO".to_string()
                    } else {
                        "OK".to_string()
                    }
                }
                Some(code) => format!("EXIT {}", code),
            },
        };

        total_files += s.files_changed;
        println!(
            " {:<4} {:<8} {:<7} {:<8} {} {:>5}  {:<8}",
            s.id,
            time_str,
            pad_or_truncate(&s.agent, 7),
            pad_or_truncate(&s.tool, 8),
            summary_col,
            s.files_changed,
            status
        );
    }

    if let Some(sess) = db.latest_session_for_project(&project_str)? {
        let tok = sess.input_tokens + sess.output_tokens + sess.cache_tokens;
        println!(
            "\nSession {} ({}) · {} steps shown · {} file changes · {} tokens · ${:.2}",
            &sess.id[..sess.id.len().min(8)],
            sess.branch.as_deref().unwrap_or("-"),
            steps.len(),
            total_files,
            format_tokens(tok),
            sess.cost_usd
        );
    }

    Ok(())
}

pub fn cmd_show(step_id: i64) -> Result<()> {
    let paths = AgentBoxPaths::resolve()?;
    let db = Database::open(&paths.db_path())?;
    let step = db
        .get_step(step_id)?
        .with_context(|| format!("Step #{} not found", step_id))?;
    let files = db.get_step_files(step_id)?;

    let time_str = Local
        .timestamp_opt(step.started_at, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| step.started_at.to_string());

    println!("Step #{} (session {} · seq {})", step.id, step.session_id, step.seq);
    println!("Time:     {}", time_str);
    println!("Agent:    {}", step.agent);
    println!("Tool:     {}", step.tool);
    println!("Summary:  {}", step.input_summary);
    println!(
        "Exit:     {}  Duration: {}ms",
        step.exit_status
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".into()),
        step.duration_ms.unwrap_or(0)
    );
    if let Some(ref gd) = step.guard_decision {
        println!(
            "Guard:    {} ({})",
            gd,
            step.guard_rule.as_deref().unwrap_or("-")
        );
    }
    println!(
        "Trees:    {} -> {}",
        step.pre_tree.as_deref().unwrap_or("-"),
        step.post_tree.as_deref().unwrap_or("-")
    );
    if step.concurrent_overlap {
        println!("Warning:  ⚠ Executed concurrently with another mutating tool call");
    }
    println!("Input:    {}", step.input_json);
    println!("\nChanged Files ({}):", files.len());
    for f in files {
        println!("  {}  {}", f.change, f.path);
    }
    Ok(())
}

pub fn cmd_diff(target: &str, patch_only: bool) -> Result<()> {
    let paths = AgentBoxPaths::resolve()?;
    let db = Database::open(&paths.db_path())?;
    let cwd = std::env::current_dir()?;
    let project_root = resolve_project_root(&cwd);
    let proj_hash = project_hash(&project_root);
    let shadow_repo = paths.shadow_repo_dir(&proj_hash);

    let (tree_a, tree_b, label) = if let Some((a_str, b_str)) = target.split_once("..") {
        let a_id: i64 = a_str.trim().parse().context("Invalid start step ID")?;
        let b_id: i64 = b_str.trim().parse().context("Invalid end step ID")?;
        let step_a = db
            .get_step(a_id)?
            .with_context(|| format!("Step #{} not found", a_id))?;
        let step_b = db
            .get_step(b_id)?
            .with_context(|| format!("Step #{} not found", b_id))?;
        let ta = step_a
            .pre_tree
            .with_context(|| format!("Step #{} has no pre_tree snapshot", a_id))?;
        let tb = step_b
            .post_tree
            .with_context(|| format!("Step #{} has no post_tree snapshot", b_id))?;
        (ta, tb, format!("steps #{}..#{}", a_id, b_id))
    } else {
        let id: i64 = target.trim().parse().context("Invalid step ID")?;
        let step = db
            .get_step(id)?
            .with_context(|| format!("Step #{} not found", id))?;
        let ta = step
            .pre_tree
            .with_context(|| format!("Step #{} has no pre_tree snapshot", id))?;
        let tb = step
            .post_tree
            .with_context(|| format!("Step #{} has no post_tree snapshot", id))?;
        (ta, tb, format!("step #{} ({})", id, step.input_summary))
    };

    if tree_a == tree_b {
        println!("No file changes in {}.", label);
        return Ok(());
    }

    let color = !patch_only && std::io::stdout().is_terminal();
    let patch = diff_patch(&shadow_repo, &tree_a, &tree_b, color)?;
    if !patch_only {
        println!("Diff for {}:\n", label);
    }
    print!("{}", patch);
    Ok(())
}

pub fn cmd_undo(
    step_arg: Option<i64>,
    to_step: Option<i64>,
    only_paths: Vec<String>,
    force: bool,
    yes: bool,
) -> Result<()> {
    let paths = AgentBoxPaths::resolve()?;
    let mut db = Database::open(&paths.db_path())?;
    let cwd = std::env::current_dir()?;
    let project_root = resolve_project_root(&cwd);
    let project_str = project_root.to_string_lossy().to_string();
    let proj_hash = project_hash(&project_root);
    let shadow_repo = paths.shadow_repo_dir(&proj_hash);
    let lock_path = paths.lock_path(&proj_hash);

    let target_step = if let Some(id) = to_step.or(step_arg) {
        db.get_step(id)?
            .with_context(|| format!("Step #{} not found", id))?
    } else {
        db.get_latest_mutating_step_for_project(&project_str)?
            .context("No undoable step with snapshots found for this project")?
    };

    let target_tree = target_step
        .pre_tree
        .clone()
        .with_context(|| format!("Step #{} has no pre_tree snapshot to restore", target_step.id))?;

    let _lock = ProjectLock::acquire(&lock_path)?;

    // Check external / manual modifications since last recorded agent snapshot
    let last_agent_tree = db.get_project_last_tree(&proj_hash)?;
    let current_snap = capture_snapshot(&project_root, &shadow_repo, None, last_agent_tree.as_deref())?;
    let current_tree = current_snap.tree_hash;

    if let Some(ref expected) = last_agent_tree {
        if expected != &current_tree {
            let (_, ext_changes) =
                detect_external_modifications(&project_root, &shadow_repo, expected)?;
            let relevant_ext: Vec<(String, String)> = if only_paths.is_empty() {
                ext_changes
            } else {
                ext_changes
                    .into_iter()
                    .filter(|(p, _)| only_paths.iter().any(|o| o == p))
                    .collect()
            };

            if !relevant_ext.is_empty() && !force {
                println!(
                    "⚠ Detected {} file(s) modified outside AgentBox since last recorded step:",
                    relevant_ext.len()
                );
                for (p, ch) in relevant_ext.iter().take(10) {
                    println!("  {}  {}", ch, p);
                }
                println!("Refusing to overwrite manual edits. Re-run with `--force` to override.");
                return Ok(());
            }
        }
    }

    let only_slice = if only_paths.is_empty() {
        None
    } else {
        Some(only_paths.as_slice())
    };
    let preview_changes = diff_trees(&shadow_repo, &current_tree, &target_tree, false)?;
    let filtered_count = if let Some(only) = only_slice {
        preview_changes
            .iter()
            .filter(|(p, _)| only.iter().any(|o| o == p))
            .count()
    } else {
        preview_changes.len()
    };

    if filtered_count == 0 {
        println!("Workspace already matches state before step #{}.", target_step.id);
        return Ok(());
    }

    if !yes {
        print!(
            "Restore {} file(s) to state before step #{} ({})? [y/N] ",
            filtered_count, target_step.id, target_step.input_summary
        );
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Restore files safely
    let applied = restore_to_tree(
        &project_root,
        &shadow_repo,
        &current_tree,
        &target_tree,
        only_slice,
    )?;

    let undo_session = format!("undo-{}", chrono::Utc::now().timestamp());
    let now_s = chrono::Utc::now().timestamp();
    let branch = detect_git_branch(&project_root);
    db.ensure_session(
        &undo_session,
        "abox",
        &project_str,
        &proj_hash,
        branch.as_deref(),
        now_s,
    )?;

    let post_snap = capture_snapshot(
        &project_root,
        &shadow_repo,
        Some(&undo_session),
        Some(&current_tree),
    )?;

    let undo_step = NewStepInput {
        session_id: undo_session,
        agent: "abox".to_string(),
        tool: "Undo".to_string(),
        input_summary: format!("undo to before #{}", target_step.id),
        input_json: serde_json::json!({
            "target_step": target_step.id,
            "only": only_paths,
            "force": force
        })
        .to_string(),
        pre_tree: Some(current_tree),
        post_tree: Some(post_snap.tree_hash.clone()),
        files_changed: applied.len() as i64,
        guard_decision: Some("allow".to_string()),
        guard_rule: None,
        exit_status: Some(0),
        duration_ms: Some(0),
        started_at: now_s,
        tokens: 0,
        cost_usd: 0.0,
        is_undo: true,
        concurrent_overlap: false,
        tool_use_id: None,
    };
    let undo_id = db.insert_step(&undo_step, &applied)?;
    db.update_project_last_tree(&proj_hash, &post_snap.tree_hash, Some(undo_id), now_s)?;

    println!(
        "✔ Restored {} file(s). (undo snapshot #{} created — `abox undo` to revert this undo)",
        applied.len(),
        undo_id
    );
    Ok(())
}

pub fn cmd_cost(by: &str, all: bool) -> Result<()> {
    let paths = AgentBoxPaths::resolve()?;
    if !paths.db_path().exists() {
        println!("No AgentBox database found. Try `abox report` to scan existing Claude Code logs.");
        return Ok(());
    }
    let db = Database::open(&paths.db_path())?;
    let cwd = std::env::current_dir()?;
    let project_root = resolve_project_root(&cwd);
    let project_filter = if all {
        None
    } else {
        Some(project_root.to_string_lossy().to_string())
    };

    let rows = db.cost_by(by, project_filter.as_deref())?;
    if rows.is_empty() {
        println!("No cost records found.");
        return Ok(());
    }

    println!(
        " {:<28} {:>8} {:>7} {:>12} {:>15}",
        by.to_uppercase(),
        "SESSIONS",
        "STEPS",
        "TOKENS",
        "API EQUIV. ($)"
    );
    for r in rows {
        let tot = r.input_tokens + r.output_tokens + r.cache_tokens;
        println!(
            " {:<28} {:>8} {:>7} {:>12} ${:>14.2}",
            pad_or_truncate(&r.key, 28),
            r.sessions,
            r.steps,
            format_tokens(tot),
            r.cost_usd
        );
    }
    println!("\nNote: Costs reflect estimated API-equivalent value (subscription plans may vary).");
    Ok(())
}

pub fn cmd_gc(days: u64, max_gb: f64) -> Result<()> {
    let paths = AgentBoxPaths::resolve()?;
    if !paths.db_path().exists() {
        println!("Nothing to clean.");
        return Ok(());
    }
    let mut db = Database::open(&paths.db_path())?;
    let cutoff = chrono::Utc::now().timestamp() - (days as i64 * 86400);
    let deleted = db.delete_sessions_older_than(cutoff)?;

    let mut repos_cleaned = 0usize;
    if paths.repos_dir().exists() {
        for entry in std::fs::read_dir(paths.repos_dir())? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if run_git_gc(&entry.path()).is_ok() {
                    repos_cleaned += 1;
                }
            }
        }
    }

    let total_bytes = dir_size_bytes(&paths.home_dir);
    let max_bytes = (max_gb * 1024.0 * 1024.0 * 1024.0) as u64;
    println!(
        "✔ GC complete: pruned {} old session(s), compacted {} shadow repo(s), total storage {:.2} MB (limit {:.1} GB)",
        deleted,
        repos_cleaned,
        total_bytes as f64 / (1024.0 * 1024.0),
        max_gb
    );
    if total_bytes > max_bytes {
        println!("⚠ Storage still exceeds {:.1} GB; run `abox gc --days 3` to prune more aggressively.", max_gb);
    }
    Ok(())
}

pub fn cmd_doctor() -> Result<()> {
    let paths = AgentBoxPaths::resolve()?;
    let cwd = std::env::current_dir()?;
    let project_root = resolve_project_root(&cwd);

    println!("AgentBox Doctor");
    println!("────────────────────────────────────────");
    println!(
        "Binary:       {}",
        current_exe_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown".into())
    );
    println!("Home dir:     {}", paths.home_dir.display());

    let git_ver = std::process::Command::new("git")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    match git_ver {
        Some(v) => println!("Git:          ✔ {}", v),
        None => println!("Git:          ✘ not found in PATH"),
    }

    for adapter in all_adapters() {
        let det = adapter.detect().unwrap_or_else(|| "not detected".into());
        let g_inst = adapter.is_installed(InstallScope::Global, &project_root);
        let p_inst = adapter.is_installed(InstallScope::Project, &project_root);
        println!(
            "Agent {}: {} | global hooks: {} | project hooks: {}",
            adapter.name(),
            det,
            if g_inst { "✔" } else { "✘" },
            if p_inst { "✔" } else { "✘" }
        );
    }

    if paths.db_path().exists() {
        let db = Database::open(&paths.db_path())?;
        let (projs, sess, steps) = db.stats_counts()?;
        let size_mb = dir_size_bytes(&paths.home_dir) as f64 / (1024.0 * 1024.0);
        println!(
            "Database:     ✔ {} project(s), {} session(s), {} step(s) | {:.2} MB total",
            projs, sess, steps, size_mb
        );
    } else {
        println!("Database:     not initialized yet (`abox init`)");
    }

    if paths.error_log_path().exists() {
        let raw = std::fs::read_to_string(paths.error_log_path()).unwrap_or_default();
        let lines: Vec<&str> = raw.lines().collect();
        println!("Error log:    {} entries", lines.len());
        for line in lines.iter().rev().take(3).rev() {
            println!("  {}", line);
        }
    } else {
        println!("Error log:    ✔ 0 errors");
    }
    Ok(())
}

/// M0 Benchmark: verify shadow-git snapshot latency on N files (default 10,000) vs PRD targets (p95 <= 80ms).
pub fn cmd_bench(files: usize, iters: usize, repo_path: Option<PathBuf>) -> Result<()> {
    let temp_root = repo_path.is_none().then(|| {
        std::env::temp_dir().join(format!("abox_bench_{}", std::process::id()))
    });
    let work_dir = repo_path.unwrap_or_else(|| temp_root.clone().unwrap());
    let shadow_dir = std::env::temp_dir().join(format!("abox_bench_shadow_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&shadow_dir);

    if let Some(ref tmp) = temp_root {
        let _ = std::fs::remove_dir_all(tmp);
        println!("Generating synthetic repo with {} files (+ excluded node_modules)...", files);
        std::fs::create_dir_all(tmp.join("node_modules/pkg")).unwrap();
        for i in 0..500 {
            std::fs::write(tmp.join(format!("node_modules/pkg/dep_{}.js", i)), "module.exports={};")?;
        }
        let dirs_count = (files / 50).max(1);
        for d in 0..dirs_count {
            std::fs::create_dir_all(tmp.join(format!("src/mod_{}", d)))?;
        }
        for i in 0..files {
            let d = i % dirs_count;
            std::fs::write(
                tmp.join(format!("src/mod_{}/file_{}.ts", d, i)),
                format!("export const v{} = {};\n", i, i),
            )?;
        }
    }

    let t_init = Instant::now();
    let baseline = capture_snapshot(&work_dir, &shadow_dir, Some("bench"), None)?;
    println!(
        "Cold baseline snapshot ({} files): {:.1} ms (tree {})",
        files,
        t_init.elapsed().as_secs_f64() * 1000.0,
        &baseline.tree_hash[..8]
    );

    let mut samples_ms = Vec::with_capacity(iters);
    let mut prev_tree = baseline.tree_hash;
    for i in 0..iters {
        // Mutate 1 file on each step like a typical agent Edit/Bash step
        let target_file = if temp_root.is_some() {
            work_dir.join("src/mod_0/file_0.ts")
        } else {
            work_dir.join(".abox_bench_probe.tmp")
        };
        std::fs::write(&target_file, format!("// iter {}\n", i))?;

        let start = Instant::now();
        let snap = capture_snapshot_with_mode(
            &work_dir,
            &shadow_dir,
            Some("bench"),
            Some(&prev_tree),
            SnapshotMode::FastIncremental,
        )?;
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        samples_ms.push(elapsed_ms);
        prev_tree = snap.tree_hash;
    }

    samples_ms.sort_by(|a, b| a.total_cmp(b));
    let p50 = percentile(&samples_ms, 0.50);
    let p95 = percentile(&samples_ms, 0.95);
    let p99 = percentile(&samples_ms, 0.99);
    let max = *samples_ms.last().unwrap_or(&0.0);

    println!("\nBenchmark Results ({} warm incremental snapshots on {} files):", iters, files);
    println!("  p50: {:.1} ms", p50);
    println!("  p95: {:.1} ms  (PRD target <= 80.0 ms)", p95);
    println!("  p99: {:.1} ms  (PRD target <= 200.0 ms)", p99);
    println!("  max: {:.1} ms", max);

    if temp_root.is_none() {
        let _ = std::fs::remove_file(work_dir.join(".abox_bench_probe.tmp"));
    }
    if let Some(tmp) = temp_root {
        let _ = std::fs::remove_dir_all(tmp);
    }
    let _ = std::fs::remove_dir_all(shadow_dir);
    Ok(())
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64) * p).ceil() as usize;
    let clamped = idx.saturating_sub(1).min(sorted.len() - 1);
    sorted[clamped]
}

fn parse_since_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    let now = chrono::Utc::now().timestamp();
    if let Some(num) = s.strip_suffix('h').and_then(|v| v.parse::<i64>().ok()) {
        return Some(now - num * 3600);
    }
    if let Some(num) = s.strip_suffix('d').and_then(|v| v.parse::<i64>().ok()) {
        return Some(now - num * 86400);
    }
    if let Some(num) = s.strip_suffix('m').and_then(|v| v.parse::<i64>().ok()) {
        return Some(now - num * 60);
    }
    None
}

fn pad_or_truncate(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count <= width {
        format!("{:<width$}", s, width = width)
    } else {
        let head: String = s.chars().take(width.saturating_sub(1)).collect();
        format!("{}…", head)
    }
}
