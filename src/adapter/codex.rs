use super::{
    AgentAdapter, HookEventKind, InstallReport, InstallScope, NormalizedHookEvent,
    TranscriptMetrics,
};
use crate::pricing::estimate_cost_usd;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct CodexAdapter;

impl CodexAdapter {
    pub fn new() -> Self {
        Self
    }

    pub fn codex_home() -> Option<PathBuf> {
        if let Ok(custom) = std::env::var("AGENTBOX_CODEX_HOME") {
            return Some(PathBuf::from(custom));
        }
        dirs::home_dir().map(|h| h.join(".codex"))
    }

    fn hooks_path(scope: InstallScope, project_root: &Path) -> Result<PathBuf> {
        match scope {
            InstallScope::Global => {
                let ch = Self::codex_home().context("Cannot locate ~/.codex")?;
                Ok(ch.join("hooks.json"))
            }
            InstallScope::Project => Ok(project_root.join(".codex").join("hooks.json")),
        }
    }

    fn is_agentbox_hook_entry(entry: &Value) -> bool {
        if let Some(hooks) = entry.get("hooks").and_then(|h| h.as_array()) {
            return hooks.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .map(|s| s.contains("hook codex") || s.contains("abox"))
                    .unwrap_or(false)
            });
        }
        false
    }
}

impl AgentAdapter for CodexAdapter {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn detect(&self) -> Option<String> {
        let dir_exists = Self::codex_home().map(|p| p.exists()).unwrap_or(false);
        let cli_version = Command::new("codex")
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

        match (cli_version, dir_exists) {
            (Some(ver), _) => Some(ver),
            (None, true) => Some("detected (~/.codex)".to_string()),
            (None, false) => None,
        }
    }

    fn install_hooks(
        &self,
        scope: InstallScope,
        project_root: &Path,
        abox_bin: &Path,
    ) -> Result<InstallReport> {
        let config_path = Self::hooks_path(scope, project_root)?;
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut backup_path = None;
        let mut root_val: Value = if config_path.exists() {
            let raw = std::fs::read_to_string(&config_path)
                .with_context(|| format!("Failed to read {}", config_path.display()))?;
            let ts = chrono::Utc::now().timestamp();
            let bak = config_path.with_extension(format!("json.abox-backup.{}", ts));
            std::fs::write(&bak, &raw)?;
            backup_path = Some(bak);
            if raw.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&raw).with_context(|| {
                    format!(
                        "Existing {} is not valid JSON; refusing to overwrite",
                        config_path.display()
                    )
                })?
            }
        } else {
            json!({})
        };

        let root_obj = root_val
            .as_object_mut()
            .context("hooks.json top-level must be a JSON object")?;
        let hooks_val = root_obj.entry("hooks").or_insert_with(|| json!({}));
        let hooks_obj = hooks_val
            .as_object_mut()
            .context("`hooks` in hooks.json must be a JSON object")?;

        let bin_str = shell_escape_path(abox_bin);
        let command_str = format!("{} hook codex", bin_str);

        let event_specs: &[(&str, Option<&str>)] = &[
            ("SessionStart", None),
            ("PreToolUse", Some("*")),
            ("PostToolUse", Some("*")),
            ("Stop", None),
            ("SessionEnd", None),
        ];

        for (event_name, matcher) in event_specs {
            let arr_val = hooks_obj.entry(*event_name).or_insert_with(|| json!([]));
            let arr = arr_val
                .as_array_mut()
                .with_context(|| format!("hooks.{} must be an array", event_name))?;

            arr.retain(|entry| !Self::is_agentbox_hook_entry(entry));

            let mut entry = serde_json::Map::new();
            if let Some(m) = matcher {
                entry.insert("matcher".to_string(), json!(m));
            }
            entry.insert(
                "hooks".to_string(),
                json!([{
                    "type": "command",
                    "command": command_str,
                    "timeout": 10
                }]),
            );
            arr.push(Value::Object(entry));
        }

        let tmp_path = config_path.with_extension("json.abox-tmp");
        let pretty = serde_json::to_string_pretty(&root_val)?;
        std::fs::write(&tmp_path, format!("{}\n", pretty))?;
        std::fs::rename(&tmp_path, &config_path)?;

        Ok(InstallReport {
            agent: self.name().to_string(),
            config_path,
            backup_path,
            post_install_note: Some(
                "Codex CLI requires approving new hooks once: run `/hooks` inside Codex (or `--dangerously-bypass-hook-trust`).".to_string(),
            ),
        })
    }

    fn uninstall_hooks(&self, scope: InstallScope, project_root: &Path) -> Result<Option<PathBuf>> {
        let config_path = Self::hooks_path(scope, project_root)?;
        if !config_path.exists() {
            return Ok(None);
        }

        let raw = std::fs::read_to_string(&config_path)?;
        if raw.trim().is_empty() {
            return Ok(None);
        }
        let mut root_val: Value = serde_json::from_str(&raw)?;
        let mut modified = false;

        if let Some(hooks_obj) = root_val.get_mut("hooks").and_then(|v| v.as_object_mut()) {
            let keys: Vec<String> = hooks_obj.keys().cloned().collect();
            for k in keys {
                if let Some(arr) = hooks_obj.get_mut(&k).and_then(|v| v.as_array_mut()) {
                    let before = arr.len();
                    arr.retain(|entry| !Self::is_agentbox_hook_entry(entry));
                    if arr.len() != before {
                        modified = true;
                    }
                    if arr.is_empty() {
                        hooks_obj.remove(&k);
                    }
                }
            }
            if hooks_obj.is_empty() {
                if let Some(root_obj) = root_val.as_object_mut() {
                    root_obj.remove("hooks");
                }
            }
        }

        if modified {
            let tmp_path = config_path.with_extension("json.abox-tmp");
            let pretty = serde_json::to_string_pretty(&root_val)?;
            std::fs::write(&tmp_path, format!("{}\n", pretty))?;
            std::fs::rename(&tmp_path, &config_path)?;
            Ok(Some(config_path))
        } else {
            Ok(None)
        }
    }

    fn is_installed(&self, scope: InstallScope, project_root: &Path) -> bool {
        let Ok(config_path) = Self::hooks_path(scope, project_root) else {
            return false;
        };
        let Ok(raw) = std::fs::read_to_string(&config_path) else {
            return false;
        };
        let Ok(val) = serde_json::from_str::<Value>(&raw) else {
            return false;
        };
        let Some(hooks) = val.get("hooks").and_then(|v| v.as_object()) else {
            return false;
        };
        hooks.values().any(|v| {
            v.as_array()
                .map(|arr| arr.iter().any(Self::is_agentbox_hook_entry))
                .unwrap_or(false)
        })
    }

    fn parse_hook_event(&self, raw_json: &str) -> Result<NormalizedHookEvent> {
        let val: Value =
            serde_json::from_str(raw_json).context("Invalid JSON received on Codex hook stdin")?;

        let event_name = val
            .get("hook_event_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let kind = match event_name {
            "SessionStart" => HookEventKind::SessionStart,
            "PreToolUse" => HookEventKind::PreToolUse,
            "PostToolUse" => HookEventKind::PostToolUse,
            "PostToolUseFailure" => HookEventKind::PostToolUseFailure,
            "Stop" => HookEventKind::Stop,
            "SessionEnd" => HookEventKind::SessionEnd,
            other => bail!("Unsupported Codex hook_event_name: {}", other),
        };

        let session_id = val
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("codex-session")
            .to_string();

        let cwd = PathBuf::from(val.get("cwd").and_then(|v| v.as_str()).unwrap_or("."));
        let transcript_path = val
            .get("transcript_path")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let tool_name = val
            .get("tool_name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let tool_use_id = val
            .get("tool_use_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let tool_input = val.get("tool_input").cloned().unwrap_or(Value::Null);

        let is_error = kind == HookEventKind::PostToolUseFailure || val.get("error").is_some();
        let exit_status = val
            .get("tool_response")
            .and_then(|r| {
                r.get("exitCode")
                    .or_else(|| r.get("exit_code"))
                    .and_then(|c| c.as_i64())
            })
            .or(if is_error { Some(1) } else { Some(0) });

        Ok(NormalizedHookEvent {
            agent: "codex".to_string(),
            kind,
            session_id,
            cwd,
            transcript_path,
            tool_name,
            tool_use_id,
            tool_input,
            exit_status,
            is_error,
        })
    }

    fn format_pretool_decision(&self, decision: &str, reason: &str) -> Option<String> {
        if decision == "allow" {
            return None;
        }
        let payload = json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": decision,
                "permissionDecisionReason": reason
            }
        });
        serde_json::to_string(&payload).ok()
    }
}

/// Best-effort Codex rollout JSONL transcript parser.
pub fn parse_codex_transcript(path: &Path) -> Result<TranscriptMetrics> {
    let file = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let reader = BufReader::new(file);

    let default_sess = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("codex")
        .to_string();

    let mut metrics = TranscriptMetrics {
        session_id: default_sess,
        primary_model: Some("codex".into()),
        ..Default::default()
    };

    for line_res in reader.lines() {
        let Ok(line) = line_res else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if let Some(sid) = val.get("session_id").and_then(|v| v.as_str()) {
            metrics.session_id = sid.to_string();
        }
        if let Some(m) = val.get("model").and_then(|v| v.as_str()) {
            metrics.primary_model = Some(m.to_string());
        }
        if let Some(usage) = val.get("usage").or_else(|| val.pointer("/payload/info/total_token_usage")) {
            let inp = usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
            let out = usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
            let cached = usage
                .get("cached_input_tokens")
                .or_else(|| usage.get("cache_read_input_tokens"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            metrics.input_tokens = metrics.input_tokens.max(inp);
            metrics.output_tokens = metrics.output_tokens.max(out);
            metrics.cache_read_tokens = metrics.cache_read_tokens.max(cached);
        }
    }

    let model = metrics.primary_model.as_deref().unwrap_or("codex");
    metrics.cost_usd = estimate_cost_usd(
        model,
        metrics.input_tokens,
        metrics.output_tokens,
        metrics.cache_write_tokens,
        metrics.cache_read_tokens,
    );
    Ok(metrics)
}

fn shell_escape_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if s.contains(' ') || s.contains('"') || s.contains('\'') {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}
