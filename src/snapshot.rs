use anyhow::{bail, Context, Result};
use fs2::FileExt;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::process::Command;

pub const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024; // 20MB per PRD F2

const DEFAULT_EXCLUDES: &[&str] = &[
    ".git/",
    ".git/**",
    "node_modules/",
    "target/",
    "dist/",
    "build/",
    ".next/",
    ".nuxt/",
    ".venv/",
    "venv/",
    "__pycache__/",
    ".pytest_cache/",
    ".DS_Store",
];

const LARGE_FILE_MARKER_START: &str = "# --- agentbox auto-excluded >20MB files ---";
const LARGE_FILE_MARKER_END: &str = "# --- end agentbox auto-excluded ---";

pub struct ProjectLock {
    _file: File,
}

impl ProjectLock {
    pub fn acquire(lock_path: &Path) -> Result<Self> {
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(lock_path)
            .with_context(|| format!("Failed to open lock file {}", lock_path.display()))?;
        file.lock_exclusive()
            .with_context(|| format!("Failed to acquire project lock {}", lock_path.display()))?;
        Ok(Self { _file: file })
    }
}

pub fn is_mutating_tool(tool: &str) -> bool {
    matches!(
        tool,
        "Edit"
            | "Write"
            | "MultiEdit"
            | "NotebookEdit"
            | "Bash"
            | "run_shell_command"
            | "write_file"
            | "edit_file"
            | "apply_patch"
            | "shell_command"
            | "exec_command"
    )
}

pub fn ensure_shadow_repo(project_root: &Path, shadow_repo: &Path) -> Result<()> {
    let head_file = shadow_repo.join("HEAD");
    if !head_file.exists() {
        std::fs::create_dir_all(shadow_repo)?;
        let out = Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg(shadow_repo)
            .output()
            .context("Failed to execute `git init --bare`")?;
        if !out.status.success() {
            bail!(
                "git init --bare failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        // Tune shadow repo for fast stat-cached snapshots
        for (k, v) in [
            ("user.name", "AgentBox"),
            ("user.email", "abox@local"),
            ("core.preloadIndex", "true"),
            ("core.untrackedCache", "true"),
            ("core.quotepath", "false"),
            ("gc.auto", "0"),
        ] {
            let _ = Command::new("git")
                .arg("--git-dir")
                .arg(shadow_repo)
                .args(["config", k, v])
                .output();
        }
    }

    sync_excludes(project_root, shadow_repo)?;
    Ok(())
}

/// Sync default excludes + `.aboxignore` into `<shadow_repo>/info/exclude`,
/// preserving any auto-excluded large file block if present.
pub fn sync_excludes(project_root: &Path, shadow_repo: &Path) -> Result<()> {
    let info_dir = shadow_repo.join("info");
    std::fs::create_dir_all(&info_dir)?;
    let exclude_path = info_dir.join("exclude");

    let mut existing_large_block = String::new();
    if let Ok(prev) = std::fs::read_to_string(&exclude_path) {
        if let (Some(s), Some(e)) = (
            prev.find(LARGE_FILE_MARKER_START),
            prev.find(LARGE_FILE_MARKER_END),
        ) {
            existing_large_block = prev[s..e + LARGE_FILE_MARKER_END.len()].to_string();
        }
    }

    let mut content = String::from("# Managed by AgentBox\n");
    for pat in DEFAULT_EXCLUDES {
        content.push_str(pat);
        content.push('\n');
    }

    let aboxignore = project_root.join(".aboxignore");
    if let Ok(raw) = std::fs::read_to_string(&aboxignore) {
        content.push_str("\n# From .aboxignore\n");
        content.push_str(&raw);
        if !raw.ends_with('\n') {
            content.push('\n');
        }
    }

    if !existing_large_block.is_empty() {
        content.push('\n');
        content.push_str(&existing_large_block);
        content.push('\n');
    }

    std::fs::write(&exclude_path, content)?;
    Ok(())
}

/// Scan workspace for files > 20MB (skipping common build/VCS dirs) and add them
/// to `<shadow_repo>/info/exclude` so git never hashes huge binaries.
pub fn scan_and_exclude_large_files(
    project_root: &Path,
    shadow_repo: &Path,
) -> Result<Vec<String>> {
    let mut large_files = Vec::new();
    let skip_dirs: HashSet<&str> = [
        ".git",
        "node_modules",
        "target",
        "dist",
        "build",
        ".next",
        ".nuxt",
        ".venv",
        "venv",
        "__pycache__",
    ]
    .into_iter()
    .collect();

    let walker = walkdir::WalkDir::new(project_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            if entry.file_type().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    if skip_dirs.contains(name) {
                        return false;
                    }
                    // Also skip nested git repos
                    if entry.path().join(".git").exists() {
                        return false;
                    }
                }
            }
            true
        });

    for entry in walker.flatten() {
        if entry.file_type().is_file() {
            if let Ok(meta) = entry.metadata() {
                if meta.len() > MAX_FILE_BYTES {
                    if let Ok(rel) = entry.path().strip_prefix(project_root) {
                        large_files.push(rel.to_string_lossy().replace('\\', "/"));
                    }
                }
            }
        }
    }

    let exclude_path = shadow_repo.join("info").join("exclude");
    let current = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    let base = if let Some(idx) = current.find(LARGE_FILE_MARKER_START) {
        current[..idx].trim_end().to_string()
    } else {
        current.trim_end().to_string()
    };

    let mut updated = base;
    if !large_files.is_empty() {
        updated.push_str("\n\n");
        updated.push_str(LARGE_FILE_MARKER_START);
        updated.push('\n');
        for f in &large_files {
            updated.push('/');
            updated.push_str(f);
            updated.push('\n');
        }
        updated.push_str(LARGE_FILE_MARKER_END);
        updated.push('\n');
    } else {
        updated.push('\n');
    }

    std::fs::write(&exclude_path, updated)?;
    Ok(large_files)
}

#[derive(Debug, Clone)]
pub struct SnapshotResult {
    pub tree_hash: String,
    #[allow(dead_code)]
    pub skipped_large_files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotMode {
    /// Full setup: sync excludes, scan >20MB files across worktree, and snapshot.
    FullBaseline,
    /// Fast incremental snapshot for hot-path hooks: reuses synced excludes unless `.aboxignore` changed.
    FastIncremental,
}

/// Capture a working tree snapshot into the shadow repository without touching the user's `.git`.
pub fn capture_snapshot(
    project_root: &Path,
    shadow_repo: &Path,
    session_id: Option<&str>,
    previous_tree: Option<&str>,
) -> Result<SnapshotResult> {
    capture_snapshot_with_mode(
        project_root,
        shadow_repo,
        session_id,
        previous_tree,
        SnapshotMode::FullBaseline,
    )
}

pub fn capture_snapshot_with_mode(
    project_root: &Path,
    shadow_repo: &Path,
    session_id: Option<&str>,
    previous_tree: Option<&str>,
    mode: SnapshotMode,
) -> Result<SnapshotResult> {
    let mut skipped_large_files = Vec::new();
    let initialized = shadow_repo.join("HEAD").exists();

    if !initialized || mode == SnapshotMode::FullBaseline {
        ensure_shadow_repo(project_root, shadow_repo)?;
        skipped_large_files = scan_and_exclude_large_files(project_root, shadow_repo)?;
    } else if aboxignore_is_newer(project_root, shadow_repo) {
        sync_excludes(project_root, shadow_repo)?;
    }

    let index_file = shadow_repo.join("index");

    let add_out = Command::new("git")
        .env("GIT_DIR", shadow_repo)
        .env("GIT_WORK_TREE", project_root)
        .env("GIT_INDEX_FILE", &index_file)
        .args(["add", "-A"])
        .output()
        .context("Failed to execute git add -A in shadow repo")?;

    if !add_out.status.success() {
        bail!(
            "shadow git add -A failed: {}",
            String::from_utf8_lossy(&add_out.stderr)
        );
    }

    let tree_out = Command::new("git")
        .env("GIT_DIR", shadow_repo)
        .env("GIT_WORK_TREE", project_root)
        .env("GIT_INDEX_FILE", &index_file)
        .arg("write-tree")
        .output()
        .context("Failed to execute git write-tree")?;

    if !tree_out.status.success() {
        bail!(
            "shadow git write-tree failed: {}",
            String::from_utf8_lossy(&tree_out.stderr)
        );
    }

    let tree_hash = String::from_utf8_lossy(&tree_out.stdout).trim().to_string();

    // Anchor tree with a lightweight commit in refs/abox/<session> only if tree changed
    // or if this is a brand new session baseline, so git gc never collects active trees.
    if let Some(sess) = session_id {
        let tree_changed = previous_tree != Some(tree_hash.as_str());
        if tree_changed || previous_tree.is_none() {
            anchor_tree_commit(shadow_repo, sess, &tree_hash)?;
        }
    }

    Ok(SnapshotResult {
        tree_hash,
        skipped_large_files,
    })
}

fn aboxignore_is_newer(project_root: &Path, shadow_repo: &Path) -> bool {
    let aboxignore = project_root.join(".aboxignore");
    let exclude = shadow_repo.join("info").join("exclude");
    let Ok(m_ab) = std::fs::metadata(&aboxignore).and_then(|m| m.modified()) else {
        return false;
    };
    let Ok(m_ex) = std::fs::metadata(&exclude).and_then(|m| m.modified()) else {
        return true;
    };
    m_ab > m_ex
}

fn anchor_tree_commit(shadow_repo: &Path, session_id: &str, tree_hash: &str) -> Result<()> {
    let sanitized_sess: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let ref_name = format!("refs/abox/{}", sanitized_sess);

    let parent = Command::new("git")
        .arg("--git-dir")
        .arg(shadow_repo)
        .args(["rev-parse", "--verify", "-q", &ref_name])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    let mut cmd = Command::new("git");
    cmd.arg("--git-dir")
        .arg(shadow_repo)
        .arg("commit-tree")
        .arg(tree_hash);
    if let Some(ref p) = parent {
        if !p.is_empty() {
            cmd.arg("-p").arg(p);
        }
    }
    cmd.args(["-m", "abox snapshot"]);

    let commit_out = cmd.output()?;
    if commit_out.status.success() {
        let commit_hash = String::from_utf8_lossy(&commit_out.stdout).trim().to_string();
        if !commit_hash.is_empty() {
            let _ = Command::new("git")
                .arg("--git-dir")
                .arg(shadow_repo)
                .args(["update-ref", &ref_name, &commit_hash])
                .output();
        }
    }
    Ok(())
}

/// Return list of (path, change_code) between two trees (`A`, `M`, `D`, `R...`).
pub fn diff_trees(
    shadow_repo: &Path,
    tree_a: &str,
    tree_b: &str,
    detect_renames: bool,
) -> Result<Vec<(String, String)>> {
    if tree_a == tree_b {
        return Ok(Vec::new());
    }
    let mut args = vec!["diff-tree", "-r", "--name-status", "--no-commit-id"];
    if !detect_renames {
        args.push("--no-renames");
    }
    args.push(tree_a);
    args.push(tree_b);

    let out = Command::new("git")
        .arg("--git-dir")
        .arg(shadow_repo)
        .args(&args)
        .output()
        .context("Failed to run git diff-tree")?;

    if !out.status.success() {
        bail!(
            "git diff-tree failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut changes = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 2 {
            let status = parts[0].chars().next().unwrap_or('M').to_string();
            let path = if parts.len() >= 3 {
                format!("{} -> {}", parts[1], parts[2])
            } else {
                parts[1].to_string()
            };
            // Never report nested gitlinks if accidentally tracked
            if path == ".git" || path.starts_with(".git/") {
                continue;
            }
            changes.push((path, status));
        }
    }
    Ok(changes)
}

pub fn diff_patch(
    shadow_repo: &Path,
    tree_a: &str,
    tree_b: &str,
    color: bool,
) -> Result<String> {
    if tree_a == tree_b {
        return Ok(String::new());
    }
    let color_flag = if color { "--color=always" } else { "--color=never" };
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(shadow_repo)
        .args(["diff", color_flag, tree_a, tree_b])
        .output()
        .context("Failed to run git diff")?;

    if !out.status.success() {
        bail!("git diff failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Detect which files have changed in the working tree relative to `expected_tree`.
pub fn detect_external_modifications(
    project_root: &Path,
    shadow_repo: &Path,
    expected_tree: &str,
) -> Result<(String, Vec<(String, String)>)> {
    let current = capture_snapshot(project_root, shadow_repo, None, Some(expected_tree))?;
    let changes = diff_trees(shadow_repo, expected_tree, &current.tree_hash, false)?;
    Ok((current.tree_hash, changes))
}

/// Restore the workspace from `current_tree` back to `target_tree`,
/// optionally restricted to `only_paths`.
/// Never touches `.git/`, gitlinks (160000), ignored files, or skipped >20MB files.
pub fn restore_to_tree(
    project_root: &Path,
    shadow_repo: &Path,
    current_tree: &str,
    target_tree: &str,
    only_paths: Option<&[String]>,
) -> Result<Vec<(String, String)>> {
    let mut changes = diff_trees(shadow_repo, current_tree, target_tree, false)?;
    if let Some(only) = only_paths {
        let wanted: HashSet<&str> = only.iter().map(|s| s.as_str()).collect();
        changes.retain(|(p, _)| wanted.contains(p.as_str()));
    }

    if changes.is_empty() {
        return Ok(Vec::new());
    }

    // Collect gitlink paths (mode 160000) in both trees to never clobber nested repos
    let gitlinks = collect_gitlinks(shadow_repo, current_tree, target_tree);

    let mut to_delete = Vec::new();
    let mut to_checkout = Vec::new();

    for (path, status) in &changes {
        if path == ".git" || path.starts_with(".git/") || gitlinks.contains(path) {
            continue;
        }
        if status == "D" {
            // Exists in current_tree, absent in target_tree -> delete from worktree
            to_delete.push(path.clone());
        } else {
            // Added or Modified in target_tree relative to current_tree -> checkout from target_tree
            to_checkout.push(path.clone());
        }
    }

    // 1. Delete files created after target_tree
    for rel in &to_delete {
        let abs = project_root.join(rel);
        if abs.exists() || abs.is_symlink() {
            let _ = std::fs::remove_file(&abs);
            // Clean up newly emptied parent directories safely up to project_root
            let mut parent = abs.parent();
            while let Some(dir) = parent {
                if dir == project_root {
                    break;
                }
                if std::fs::remove_dir(dir).is_err() {
                    break;
                }
                parent = dir.parent();
            }
        }
    }

    // 2. Checkout files from target_tree using an isolated temp index so shadow index stays clean
    if !to_checkout.is_empty() {
        let restore_index = shadow_repo.join("restore_index");
        let _ = std::fs::remove_file(&restore_index);

        for chunk in to_checkout.chunks(200) {
            let mut cmd = Command::new("git");
            cmd.env("GIT_DIR", shadow_repo)
                .env("GIT_WORK_TREE", project_root)
                .env("GIT_INDEX_FILE", &restore_index)
                .args(["checkout", "-f", target_tree, "--"]);
            for p in chunk {
                cmd.arg(p);
            }
            let out = cmd
                .output()
                .context("Failed to execute git checkout during restore")?;
            if !out.status.success() {
                let _ = std::fs::remove_file(&restore_index);
                bail!(
                    "git checkout restore failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }
        let _ = std::fs::remove_file(&restore_index);
    }

    Ok(changes)
}

fn collect_gitlinks(shadow_repo: &Path, tree_a: &str, tree_b: &str) -> HashSet<String> {
    let mut out_set = HashSet::new();
    for tree in [tree_a, tree_b] {
        if let Ok(out) = Command::new("git")
            .arg("--git-dir")
            .arg(shadow_repo)
            .args(["ls-tree", "-r", tree])
            .output()
        {
            if out.status.success() {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    if line.starts_with("160000 ") {
                        if let Some((_, path)) = line.split_once('\t') {
                            out_set.insert(path.to_string());
                        }
                    }
                }
            }
        }
    }
    out_set
}

pub fn delete_session_ref(shadow_repo: &Path, session_id: &str) {
    let sanitized_sess: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let ref_name = format!("refs/abox/{}", sanitized_sess);
    let _ = Command::new("git")
        .arg("--git-dir")
        .arg(shadow_repo)
        .args(["update-ref", "-d", &ref_name])
        .output();
}

pub fn run_git_gc(shadow_repo: &Path) -> Result<()> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(shadow_repo)
        .args(["gc", "--prune=now"])
        .output()
        .context("Failed to run git gc on shadow repo")?;
    if !out.status.success() {
        bail!("git gc failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

pub fn dir_size_bytes(path: &Path) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}
