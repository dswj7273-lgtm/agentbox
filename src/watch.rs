use crate::db::{Database, NewStepInput};
use crate::paths::{detect_git_branch, project_hash, resolve_project_root, AgentBoxPaths};
use crate::snapshot::{capture_snapshot_with_mode, diff_trees, ProjectLock, SnapshotMode};
use anyhow::Result;
use std::path::Path;
use std::time::Duration;

/// Watch project filesystem changes for agents without native hooks (e.g., Cursor, Windsurf).
/// Captures incremental shadow-git snapshots whenever tracked files change.
pub fn cmd_watch(agent: &str, interval_ms: u64, max_cycles: Option<usize>) -> Result<()> {
    let paths = AgentBoxPaths::resolve()?;
    paths.ensure_dirs()?;
    let mut db = Database::open(&paths.db_path())?;

    let cwd = std::env::current_dir()?;
    let project_root = resolve_project_root(&cwd);
    let project_str = project_root.to_string_lossy().to_string();
    let proj_hash = project_hash(&project_root);
    let shadow_repo = paths.shadow_repo_dir(&proj_hash);
    let lock_path = paths.lock_path(&proj_hash);

    let session_id = format!("watch-{}-{}", agent, chrono::Utc::now().timestamp());
    let branch = detect_git_branch(&project_root);
    let now_s = chrono::Utc::now().timestamp();
    db.ensure_session(
        &session_id,
        agent,
        &project_str,
        &proj_hash,
        branch.as_deref(),
        now_s,
    )?;

    // Capture initial baseline
    let mut last_tree = {
        let _lock = ProjectLock::acquire(&lock_path)?;
        let prev = db.get_project_last_tree(&proj_hash)?;
        let snap = capture_snapshot_with_mode(
            &project_root,
            &shadow_repo,
            Some(&session_id),
            prev.as_deref(),
            SnapshotMode::FullBaseline,
        )?;
        db.upsert_project_baseline(
            &proj_hash,
            &project_str,
            Some(&snap.tree_hash),
            Some(&snap.tree_hash),
            now_s,
        )?;
        snap.tree_hash
    };

    println!(
        "✔ Watching {} for `{}` changes every {}ms (session {}, baseline {})",
        project_root.display(),
        agent,
        interval_ms,
        &session_id[..session_id.len().min(18)],
        &last_tree[..8]
    );

    let mut cycle = 0usize;
    loop {
        if let Some(limit) = max_cycles {
            if cycle >= limit {
                break;
            }
        }
        if max_cycles.is_none() || cycle > 0 || interval_ms > 0 {
            std::thread::sleep(Duration::from_millis(interval_ms));
        }
        cycle += 1;

        if let Some((new_tree, step_id, count)) = poll_watch_once(
            &mut db,
            &project_root,
            &project_str,
            &proj_hash,
            &shadow_repo,
            &lock_path,
            &session_id,
            agent,
            &last_tree,
        )? {
            println!(
                "  [step #{}] {} captured {} file change(s) -> tree {}",
                step_id,
                agent,
                count,
                &new_tree[..8]
            );
            last_tree = new_tree;
        }
    }

    db.finish_session(&session_id, chrono::Utc::now().timestamp(), 0, 0, 0, 0.0)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn poll_watch_once(
    db: &mut Database,
    project_root: &Path,
    _project_str: &str,
    proj_hash: &str,
    shadow_repo: &Path,
    lock_path: &Path,
    session_id: &str,
    agent: &str,
    last_tree: &str,
) -> Result<Option<(String, i64, usize)>> {
    let _lock = ProjectLock::acquire(lock_path)?;
    let snap = capture_snapshot_with_mode(
        project_root,
        shadow_repo,
        Some(session_id),
        Some(last_tree),
        SnapshotMode::FastIncremental,
    )?;

    if snap.tree_hash == last_tree {
        return Ok(None);
    }

    let changes = diff_trees(shadow_repo, last_tree, &snap.tree_hash, true)?;
    if changes.is_empty() {
        return Ok(None);
    }

    let now_s = chrono::Utc::now().timestamp();
    let summary = summarize_changes(&changes);
    let files_json = serde_json::json!({
        "source": "fs_watch",
        "files": changes.iter().map(|(p, c)| format!("{} {}", c, p)).collect::<Vec<_>>()
    })
    .to_string();

    let step = NewStepInput {
        session_id: session_id.to_string(),
        agent: agent.to_string(),
        tool: "FsWatch".to_string(),
        input_summary: summary,
        input_json: files_json,
        pre_tree: Some(last_tree.to_string()),
        post_tree: Some(snap.tree_hash.clone()),
        files_changed: changes.len() as i64,
        guard_decision: Some("allow".to_string()),
        guard_rule: None,
        exit_status: Some(0),
        duration_ms: Some(0),
        started_at: now_s,
        tokens: 0,
        cost_usd: 0.0,
        is_undo: false,
        concurrent_overlap: false,
        tool_use_id: None,
    };

    let step_id = db.insert_step(&step, &changes)?;
    db.update_project_last_tree(proj_hash, &snap.tree_hash, Some(step_id), now_s)?;
    Ok(Some((snap.tree_hash, step_id, changes.len())))
}

fn summarize_changes(changes: &[(String, String)]) -> String {
    if changes.len() == 1 {
        format!("{} {}", changes[0].1, changes[0].0)
    } else {
        format!("{} files ({} ...)", changes.len(), changes[0].0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::restore_to_tree;

    #[test]
    fn watch_captures_fs_edits_and_restores() {
        let root = std::env::temp_dir().join(format!("abox_watch_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let proj = root.join("proj");
        let abox_home = root.join("abox");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("index.ts"), "v1\n").unwrap();

        let paths = AgentBoxPaths {
            home_dir: abox_home,
        };
        paths.ensure_dirs().unwrap();
        let mut db = Database::open(&paths.db_path()).unwrap();
        let ph = project_hash(&proj);
        let shadow = paths.shadow_repo_dir(&ph);
        let lock = paths.lock_path(&ph);

        db.ensure_session("s-cur", "cursor", &proj.to_string_lossy(), &ph, None, 100)
            .unwrap();
        let base = capture_snapshot_with_mode(
            &proj,
            &shadow,
            Some("s-cur"),
            None,
            SnapshotMode::FullBaseline,
        )
        .unwrap();

        let none_res = poll_watch_once(
            &mut db,
            &proj,
            &proj.to_string_lossy(),
            &ph,
            &shadow,
            &lock,
            "s-cur",
            "cursor",
            &base.tree_hash,
        )
        .unwrap();
        assert!(none_res.is_none());

        std::fs::write(proj.join("index.ts"), "v2\n").unwrap();
        std::fs::write(proj.join("new.ts"), "added\n").unwrap();

        let some_res = poll_watch_once(
            &mut db,
            &proj,
            &proj.to_string_lossy(),
            &ph,
            &shadow,
            &lock,
            "s-cur",
            "cursor",
            &base.tree_hash,
        )
        .unwrap()
        .expect("should detect 2 changed files");
        assert_eq!(some_res.2, 2);

        restore_to_tree(&proj, &shadow, &some_res.0, &base.tree_hash, None).unwrap();
        assert_eq!(std::fs::read_to_string(proj.join("index.ts")).unwrap(), "v1\n");
        assert!(!proj.join("new.ts").exists());

        let _ = std::fs::remove_dir_all(&root);
    }
}
