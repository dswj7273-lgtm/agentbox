use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct AgentBoxPaths {
    pub home_dir: PathBuf,
}

impl AgentBoxPaths {
    pub fn resolve() -> Result<Self> {
        let home_dir = if let Ok(custom) = std::env::var("AGENTBOX_HOME") {
            PathBuf::from(custom)
        } else {
            let home = dirs::home_dir().context("Failed to determine user home directory")?;
            home.join(".agentbox")
        };
        Ok(Self { home_dir })
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.home_dir)?;
        std::fs::create_dir_all(self.repos_dir())?;
        std::fs::create_dir_all(self.locks_dir())?;
        Ok(())
    }

    pub fn db_path(&self) -> PathBuf {
        self.home_dir.join("agentbox.db")
    }

    pub fn repos_dir(&self) -> PathBuf {
        self.home_dir.join("repos")
    }

    pub fn locks_dir(&self) -> PathBuf {
        self.home_dir.join("locks")
    }

    pub fn error_log_path(&self) -> PathBuf {
        self.home_dir.join("error.log")
    }

    pub fn global_policy_path(&self) -> PathBuf {
        self.home_dir.join("policy.yaml")
    }

    pub fn shadow_repo_dir(&self, project_hash: &str) -> PathBuf {
        self.repos_dir().join(project_hash)
    }

    pub fn lock_path(&self, project_hash: &str) -> PathBuf {
        self.locks_dir().join(format!("{}.lock", project_hash))
    }
}

/// Resolve the project root for a given working directory.
/// Uses the Git top-level directory if available; otherwise canonicalizes `cwd`.
pub fn resolve_project_root(cwd: &Path) -> PathBuf {
    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    if let Ok(output) = Command::new("git")
        .arg("-C")
        .arg(&canonical_cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
        if output.status.success() {
            let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !root.is_empty() {
                let root_path = PathBuf::from(root);
                return std::fs::canonicalize(&root_path).unwrap_or(root_path);
            }
        }
    }
    canonical_cwd
}

pub fn project_hash(project_root: &Path) -> String {
    let canonical = std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    hex_encode(&digest[..8])
}

pub fn detect_git_branch(project_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if output.status.success() {
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !branch.is_empty() {
            return Some(branch);
        }
    }
    None
}

pub fn current_exe_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("Cannot locate current abox executable")?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut s, "{:02x}", b);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_hash_is_deterministic_16_hex() {
        let h1 = project_hash(Path::new("/tmp"));
        let h2 = project_hash(Path::new("/tmp"));
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
