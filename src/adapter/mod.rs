pub mod claude;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallScope {
    Global,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEventKind {
    SessionStart,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    Stop,
    SessionEnd,
}

#[derive(Debug, Clone)]
pub struct NormalizedHookEvent {
    pub agent: String,
    pub kind: HookEventKind,
    pub session_id: String,
    pub cwd: PathBuf,
    pub transcript_path: Option<PathBuf>,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub tool_input: serde_json::Value,
    pub exit_status: Option<i64>,
    #[allow(dead_code)]
    pub is_error: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TranscriptMetrics {
    pub session_id: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_write_tokens: i64,
    pub cache_read_tokens: i64,
    pub cost_usd: f64,
    pub primary_model: Option<String>,
    pub shell_commands: Vec<String>,
    pub edited_files: Vec<String>,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub project_cwd: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InstallReport {
    #[allow(dead_code)]
    pub agent: String,
    pub config_path: PathBuf,
    pub backup_path: Option<PathBuf>,
}

pub trait AgentAdapter {
    fn name(&self) -> &'static str;
    fn detect(&self) -> Option<String>;
    fn install_hooks(
        &self,
        scope: InstallScope,
        project_root: &Path,
        abox_bin: &Path,
    ) -> Result<InstallReport>;
    fn uninstall_hooks(&self, scope: InstallScope, project_root: &Path) -> Result<Option<PathBuf>>;
    fn is_installed(&self, scope: InstallScope, project_root: &Path) -> bool;
    fn parse_hook_event(&self, raw_json: &str) -> Result<NormalizedHookEvent>;
    fn format_pretool_decision(&self, decision: &str, reason: &str) -> Option<String>;
}

pub fn get_adapter(name: &str) -> Option<Box<dyn AgentAdapter>> {
    match name {
        "claude" | "claude-code" => Some(Box::new(claude::ClaudeAdapter::new())),
        _ => None,
    }
}

pub fn all_adapters() -> Vec<Box<dyn AgentAdapter>> {
    vec![Box::new(claude::ClaudeAdapter::new())]
}
