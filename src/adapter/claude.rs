use super::{
    AgentAdapter, HookEventKind, InstallReport, InstallScope, NormalizedHookEvent,
    TranscriptMetrics,
};
use crate::pricing::estimate_cost_usd;
use anyhow::{bail, Context, Result};
use chrono::DateTime;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct ClaudeAdapter;

impl ClaudeAdapter {
    pub fn new() -> Self {
        Self
    }

    pub fn claude_home() -> Option<PathBuf> {
        if let Ok(custom) = std::env::var("AGENTBOX_CLAUDE_HOME") {
            return Some(PathBuf::from(custom));
        }
        dirs::home_dir().map(|h| h.join(".claude"))
    }

    fn settings_path(scope: InstallScope, project_root: &Path) -> Result<PathBuf> {
        match scope {
            InstallScope::Global => {
                let ch = Self::claude_home().context("Cannot locate ~/.claude")?;
                Ok(ch.join("settings.json"))
            }
            InstallScope::Project => Ok(project_root.join(".claude").join("settings.json")),
        }
    }

    fn is_agentbox_hook_entry(entry: &Value) -> bool {
        if let Some(hooks) = entry.get("hooks").and_then(|h| h.as_array()) {
            return hooks.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .map(|s| s.contains("hook claude") || s.contains("abox"))
                    .unwrap_or(false)
            });
        }
        false
    }
}

impl AgentAdapter for ClaudeAdapter {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn detect(&self) -> Option<String> {
        let claude_dir_exists = Self::claude_home().map(|p| p.exists()).unwrap_or(false);
        let cli_version = Command::new("claude")
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

        match (cli_version, claude_dir_exists) {
            (Some(ver), _) => Some(ver),
            (None, true) => Some("detected (~/.claude)".to_string()),
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
        let command_str = format!("{} hook claude", bin_str);

        let event_specs: &[(&str, Option<&str>)] = &[
            ("SessionStart", None),
            ("PreToolUse", Some("*")),
            ("PostToolUse", Some("*")),
            ("PostToolUseFailure", Some("*")),
            ("Stop", None),
            ("SessionEnd", None),
        ];

        for (event_name, matcher) in event_specs {
            let arr_val = hooks_obj.entry(*event_name).or_insert_with(|| json!([]));
            let arr = arr_val
                .as_array_mut()
                .with_context(|| format!("hooks.{} must be an array", event_name))?;

            // Remove any stale AgentBox entries before inserting updated one
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

        // Atomic write via temp file + rename
        let tmp_path = config_path.with_extension("json.abox-tmp");
        let pretty = serde_json::to_string_pretty(&root_val)?;
        std::fs::write(&tmp_path, format!("{}\n", pretty))?;
        std::fs::rename(&tmp_path, &config_path)?;

        Ok(InstallReport {
            agent: self.name().to_string(),
            config_path,
            backup_path,
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
            serde_json::from_str(raw_json).context("Invalid JSON received on hook stdin")?;

        let event_name = val
            .get("hook_event_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let kind = match event_name {
            "SessionStart" => HookEventKind::SessionStart,
            "PreToolUse" => HookEventKind::PreToolUse,
            "PostToolUse" => HookEventKind::PostToolUse,
            "PostToolUseFailure" => HookEventKind::PostToolUseFailure,
            "Stop" | "SubagentStop" => HookEventKind::Stop,
            "SessionEnd" => HookEventKind::SessionEnd,
            other => bail!("Unsupported Claude hook_event_name: {}", other),
        };

        let session_id = val
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown-session")
            .to_string();

        let cwd_str = val
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or(".");
        let cwd = PathBuf::from(cwd_str);

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

        let is_error = kind == HookEventKind::PostToolUseFailure
            || val
                .get("is_interrupt")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            || val.get("error").is_some();

        // Extract exit code from tool_response if available (Claude Bash tool puts exit code or stderr)
        let exit_status = val
            .get("tool_response")
            .and_then(|r| {
                r.get("exitCode")
                    .or_else(|| r.get("exit_code"))
                    .and_then(|c| c.as_i64())
            })
            .or(if is_error { Some(1) } else { Some(0) });

        Ok(NormalizedHookEvent {
            agent: "claude".to_string(),
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

fn shell_escape_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if s.contains(' ') || s.contains('"') || s.contains('\'') {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

/// Stream-parse a Claude Code JSONL transcript file, deduplicating streaming message snapshots by `message.id`
/// and event `uuid` so token totals and tool counts are accurate.
pub fn parse_claude_transcript(path: &Path) -> Result<TranscriptMetrics> {
    let file = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let reader = BufReader::new(file);

    let default_sess = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let mut metrics = TranscriptMetrics {
        session_id: default_sess,
        ..Default::default()
    };

    #[derive(Default)]
    struct MsgUsage {
        model: String,
        input: i64,
        output: i64,
        cache_write: i64,
        cache_read: i64,
    }

    let mut seen_uuids: HashSet<String> = HashSet::new();
    let mut seen_tool_ids: HashSet<String> = HashSet::new();
    let mut msg_usages: HashMap<String, MsgUsage> = HashMap::new();
    let mut edited_set: HashSet<String> = HashSet::new();
    let mut line_idx: usize = 0;

    for line_res in reader.lines() {
        let line = match line_res {
            Ok(l) => l,
            Err(_) => continue,
        };
        line_idx += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };

        if let Some(uuid) = val.get("uuid").and_then(|v| v.as_str()) {
            if !seen_uuids.insert(uuid.to_string()) {
                continue;
            }
        }

        if let Some(sid) = val.get("sessionId").and_then(|v| v.as_str()) {
            metrics.session_id = sid.to_string();
        }
        if metrics.project_cwd.is_none() {
            if let Some(cwd) = val.get("cwd").and_then(|v| v.as_str()) {
                metrics.project_cwd = Some(cwd.to_string());
            }
        }
        if let Some(ts_str) = val.get("timestamp").and_then(|v| v.as_str()) {
            if let Ok(dt) = DateTime::parse_from_rfc3339(ts_str) {
                let ts = dt.timestamp();
                metrics.started_at = Some(metrics.started_at.map_or(ts, |m| m.min(ts)));
                metrics.ended_at = Some(metrics.ended_at.map_or(ts, |m| m.max(ts)));
            }
        }

        let Some(msg) = val.get("message") else {
            continue;
        };

        if let Some(model) = msg.get("model").and_then(|v| v.as_str()) {
            if !model.is_empty() && model != "<synthetic>" {
                metrics.primary_model = Some(model.to_string());
            }
        }

        // Token usage deduplicated by message.id (streaming snapshots reuse same message.id; keep max/latest)
        if let Some(usage) = msg.get("usage") {
            let msg_id = msg
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("line-{}", line_idx));
            let model = msg
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("claude-sonnet")
                .to_string();

            let input = usage
                .get("input_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let output = usage
                .get("output_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let cache_write = usage
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let cache_read = usage
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);

            let entry = msg_usages.entry(msg_id).or_default();
            entry.model = model;
            entry.input = entry.input.max(input);
            entry.output = entry.output.max(output);
            entry.cache_write = entry.cache_write.max(cache_write);
            entry.cache_read = entry.cache_read.max(cache_read);
        }

        // Tool uses inside assistant message content
        if let Some(content_arr) = msg.get("content").and_then(|v| v.as_array()) {
            for block in content_arr {
                if block.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                    continue;
                }
                if let Some(tid) = block.get("id").and_then(|v| v.as_str()) {
                    if !seen_tool_ids.insert(tid.to_string()) {
                        continue;
                    }
                }
                let tname = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let tinput = block.get("input").unwrap_or(&Value::Null);
                match tname {
                    "Bash" => {
                        if let Some(cmd) = tinput.get("command").and_then(|v| v.as_str()) {
                            metrics.shell_commands.push(cmd.to_string());
                        }
                    }
                    "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
                        if let Some(fp) = tinput
                            .get("file_path")
                            .or_else(|| tinput.get("notebook_path"))
                            .and_then(|v| v.as_str())
                        {
                            edited_set.insert(fp.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    for usage in msg_usages.values() {
        metrics.input_tokens += usage.input;
        metrics.output_tokens += usage.output;
        metrics.cache_write_tokens += usage.cache_write;
        metrics.cache_read_tokens += usage.cache_read;
        metrics.cost_usd += estimate_cost_usd(
            &usage.model,
            usage.input,
            usage.output,
            usage.cache_write,
            usage.cache_read,
        );
    }
    metrics.edited_files = edited_set.into_iter().collect();
    metrics.edited_files.sort();
    Ok(metrics)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_claude_jsonl_deduped() {
        let dir = std::env::temp_dir().join("abox_test_transcript");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("sess-1.jsonl");
        let mut f = File::create(&file).unwrap();
        writeln!(f, r#"{{"type":"assistant","uuid":"u1","sessionId":"s1","cwd":"/tmp/app","timestamp":"2026-11-17T01:00:00Z","message":{{"id":"m1","model":"claude-sonnet-4-5","content":[{{"type":"tool_use","id":"t1","name":"Bash","input":{{"command":"npm test"}}}},{{"type":"tool_use","id":"t2","name":"Edit","input":{{"file_path":"src/main.rs"}}}}],"usage":{{"input_tokens":1000,"output_tokens":50,"cache_creation_input_tokens":0,"cache_read_input_tokens":5000}}}}}}"#).unwrap();
        // Streaming update for same message id m1 with final output_tokens=200
        writeln!(f, r#"{{"type":"assistant","uuid":"u2","sessionId":"s1","cwd":"/tmp/app","timestamp":"2026-11-17T01:00:05Z","message":{{"id":"m1","model":"claude-sonnet-4-5","content":[{{"type":"tool_use","id":"t1","name":"Bash","input":{{"command":"npm test"}}}}],"usage":{{"input_tokens":1000,"output_tokens":200,"cache_creation_input_tokens":0,"cache_read_input_tokens":5000}}}}}}"#).unwrap();

        let m = parse_claude_transcript(&file).unwrap();
        assert_eq!(m.session_id, "s1");
        assert_eq!(m.input_tokens, 1000);
        assert_eq!(m.output_tokens, 200); // deduped by message.id!
        assert_eq!(m.cache_read_tokens, 5000);
        assert_eq!(m.shell_commands, vec!["npm test"]);
        assert_eq!(m.edited_files, vec!["src/main.rs"]);
        assert!(m.cost_usd > 0.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_and_uninstall_preserves_existing_hooks() {
        let dir = std::env::temp_dir().join("abox_test_install");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        let settings = dir.join(".claude").join("settings.json");
        std::fs::write(
            &settings,
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"my-custom-linter"}]}]}}"#,
        )
        .unwrap();

        let adapter = ClaudeAdapter::new();
        adapter
            .install_hooks(InstallScope::Project, &dir, Path::new("/usr/local/bin/abox"))
            .unwrap();
        let after_install = std::fs::read_to_string(&settings).unwrap();
        assert!(after_install.contains("my-custom-linter"));
        assert!(after_install.contains("/usr/local/bin/abox hook claude"));
        assert!(after_install.contains("PostToolUseFailure"));

        adapter
            .uninstall_hooks(InstallScope::Project, &dir)
            .unwrap();
        let after_uninstall = std::fs::read_to_string(&settings).unwrap();
        assert!(after_uninstall.contains("my-custom-linter"));
        assert!(!after_uninstall.contains("/usr/local/bin/abox"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
