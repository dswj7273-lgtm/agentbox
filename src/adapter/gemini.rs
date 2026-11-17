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

pub struct GeminiAdapter;

impl GeminiAdapter {
    pub fn new() -> Self {
        Self
    }

    pub fn gemini_home() -> Option<PathBuf> {
        if let Ok(custom) = std::env::var("AGENTBOX_GEMINI_HOME") {
            return Some(PathBuf::from(custom));
        }
        dirs::home_dir().map(|h| h.join(".gemini"))
    }

    fn settings_path(scope: InstallScope, project_root: &Path) -> Result<PathBuf> {
        match scope {
            InstallScope::Global => {
                let gh = Self::gemini_home().context("Cannot locate ~/.gemini")?;
                Ok(gh.join("settings.json"))
            }
            InstallScope::Project => Ok(project_root.join(".gemini").join("settings.json")),
        }
    }

    fn is_agentbox_hook_entry(entry: &Value) -> bool {
        if let Some(hooks) = entry.get("hooks").and_then(|h| h.as_array()) {
            return hooks.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .map(|s| s.contains("hook gemini") || s.contains("abox"))
                    .unwrap_or(false)
            });
        }
        false
    }
}

impl AgentAdapter for GeminiAdapter {
    fn name(&self) -> &'static str {
        "gemini"
    }

    fn detect(&self) -> Option<String> {
        let dir_exists = Self::gemini_home().map(|p| p.exists()).unwrap_or(false);
        let cli_version = Command::new("gemini")
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

        match (cli_version, dir_exists) {
            (Some(ver), _) => Some(ver),
            (None, true) => Some("detected (~/.gemini)".to_string()),
            (None, false) => None,
        }
    }

    fn install_hooks(
        &self,
        scope: InstallScope,
        project_root: &Path,
        abox_bin: &Path,
    ) -> Result<InstallReport> {
        let config_path = Self::settings_path(scope, project_root)?;
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
            .context("settings.json top-level must be a JSON object")?;
        let hooks_val = root_obj.entry("hooks").or_insert_with(|| json!({}));
        let hooks_obj = hooks_val
            .as_object_mut()
            .context("`hooks` in settings.json must be a JSON object")?;

        let bin_str = shell_escape_path(abox_bin);
        let command_str = format!("{} hook gemini", bin_str);

        // Gemini CLI uses BeforeTool / AfterTool instead of PreToolUse / PostToolUse
        let event_specs: &[(&str, Option<&str>)] = &[
            ("SessionStart", None),
            ("BeforeTool", Some("*")),
            ("AfterTool", Some("*")),
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
                    "timeout": 10000
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
            post_install_note: None,
        })
    }

    fn uninstall_hooks(&self, scope: InstallScope, project_root: &Path) -> Result<Option<PathBuf>> {
        let config_path = Self::settings_path(scope, project_root)?;
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
        let Ok(config_path) = Self::settings_path(scope, project_root) else {
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
            serde_json::from_str(raw_json).context("Invalid JSON received on Gemini hook stdin")?;

        let event_name = val
            .get("hook_event_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let kind = match event_name {
            "SessionStart" => HookEventKind::SessionStart,
            "BeforeTool" => HookEventKind::PreToolUse,
            "AfterTool" => HookEventKind::PostToolUse,
            "SessionEnd" => HookEventKind::SessionEnd,
            other => bail!("Unsupported Gemini hook_event_name: {}", other),
        };

        let session_id = val
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("gemini-session")
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
            .or_else(|| val.get("call_id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let tool_input = val.get("tool_input").cloned().unwrap_or(Value::Null);

        let is_error = val
            .get("tool_response")
            .and_then(|r| r.get("error"))
            .is_some()
            || val.get("error").is_some();

        let exit_status = val
            .get("tool_response")
            .and_then(|r| r.get("exitCode").or_else(|| r.get("exit_code")).and_then(|c| c.as_i64()))
            .or(if is_error { Some(1) } else { Some(0) });

        Ok(NormalizedHookEvent {
            agent: "gemini".to_string(),
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
        match decision {
            "allow" => None,
            "ask" => serde_json::to_string(&json!({
                "decision": "ask",
                "systemMessage": reason
            }))
            .ok(),
            _ => serde_json::to_string(&json!({
                "decision": "deny",
                "reason": reason
            }))
            .ok(),
        }
    }
}

/// Best-effort Gemini CLI transcript parser (supports JSON array or JSONL).
pub fn parse_gemini_transcript(path: &Path) -> Result<TranscriptMetrics> {
    let default_sess = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("gemini")
        .to_string();
    let mut metrics = TranscriptMetrics {
        session_id: default_sess,
        primary_model: Some("gemini".into()),
        ..Default::default()
    };

    let file = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    for line_res in reader.lines() {
        let Ok(line) = line_res else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if let Some(m) = val.get("model").and_then(|v| v.as_str()) {
            metrics.primary_model = Some(m.to_string());
        }
        if let Some(u) = val.get("usageMetadata").or_else(|| val.get("usage")) {
            metrics.input_tokens += u
                .get("promptTokenCount")
                .or_else(|| u.get("input_tokens"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            metrics.output_tokens += u
                .get("candidatesTokenCount")
                .or_else(|| u.get("output_tokens"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            metrics.cache_read_tokens += u
                .get("cachedContentTokenCount")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
        }
    }

    let model = metrics.primary_model.as_deref().unwrap_or("gemini");
    metrics.cost_usd = estimate_cost_usd(
        model,
        metrics.input_tokens,
        metrics.output_tokens,
        0,
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
