use super::{
    AgentAdapter, HookEventKind, InstallReport, InstallScope, NormalizedHookEvent,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct OpenCodeAdapter;

impl OpenCodeAdapter {
    pub fn new() -> Self {
        Self
    }

    pub fn opencode_home() -> Option<PathBuf> {
        if let Ok(custom) = std::env::var("AGENTBOX_OPENCODE_HOME") {
            return Some(PathBuf::from(custom));
        }
        dirs::home_dir().map(|h| h.join(".config").join("opencode"))
    }

    fn plugin_path(scope: InstallScope, project_root: &Path) -> Result<PathBuf> {
        match scope {
            InstallScope::Global => {
                let oh = Self::opencode_home().context("Cannot locate ~/.config/opencode")?;
                Ok(oh.join("plugins").join("agentbox.js"))
            }
            InstallScope::Project => Ok(project_root.join(".opencode").join("plugins").join("agentbox.js")),
        }
    }
}

impl AgentAdapter for OpenCodeAdapter {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn detect(&self) -> Option<String> {
        let dir_exists = Self::opencode_home().map(|p| p.exists()).unwrap_or(false);
        let cli_version = Command::new("opencode")
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

        match (cli_version, dir_exists) {
            (Some(ver), _) => Some(ver),
            (None, true) => Some("detected (~/.config/opencode)".to_string()),
            (None, false) => None,
        }
    }

    fn install_hooks(
        &self,
        scope: InstallScope,
        project_root: &Path,
        abox_bin: &Path,
    ) -> Result<InstallReport> {
        let plugin_path = Self::plugin_path(scope, project_root)?;
        if let Some(parent) = plugin_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut backup_path = None;
        if plugin_path.exists() {
            let raw = std::fs::read_to_string(&plugin_path)?;
            if !raw.contains("Managed by AgentBox") {
                let ts = chrono::Utc::now().timestamp();
                let bak = plugin_path.with_extension(format!("js.abox-backup.{}", ts));
                std::fs::write(&bak, raw)?;
                backup_path = Some(bak);
            }
        }

        let bin_escaped = abox_bin.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
        let plugin_js = format!(
            r#"// Managed by AgentBox — do not edit manually
import {{ spawnSync }} from "child_process";

const ABOX_BIN = "{bin}";

function invokeAbox(payload) {{
  try {{
    const res = spawnSync(ABOX_BIN, ["hook", "opencode"], {{
      input: JSON.stringify(payload),
      encoding: "utf8",
      timeout: 10000,
    }});
    if (res.stdout && res.stdout.trim()) {{
      return JSON.parse(res.stdout.trim());
    }}
  }} catch (_) {{
    // Fail-open: never block OpenCode if AgentBox encounters an internal error
  }}
  return null;
}}

export const AgentBoxPlugin = async ({{ directory }}) => {{
  return {{
    "tool.execute.before": async (input, output) => {{
      const verdict = invokeAbox({{
        hook_event_name: "tool.execute.before",
        session_id: input.sessionID || "opencode-session",
        cwd: directory || process.cwd(),
        tool_name: input.tool,
        tool_use_id: input.callID,
        tool_input: output.args || {{}},
      }});
      if (verdict && (verdict.decision === "deny" || verdict.decision === "ask")) {{
        throw new Error(verdict.reason || "Blocked by AgentBox policy");
      }}
    }},
    "tool.execute.after": async (input, output) => {{
      invokeAbox({{
        hook_event_name: "tool.execute.after",
        session_id: input.sessionID || "opencode-session",
        cwd: directory || process.cwd(),
        tool_name: input.tool,
        tool_use_id: input.callID,
        tool_input: (output && output.args) || {{}},
      }});
    }},
  }};
}};
"#,
            bin = bin_escaped
        );

        std::fs::write(&plugin_path, plugin_js)?;

        Ok(InstallReport {
            agent: self.name().to_string(),
            config_path: plugin_path,
            backup_path,
            post_install_note: None,
        })
    }

    fn uninstall_hooks(&self, scope: InstallScope, project_root: &Path) -> Result<Option<PathBuf>> {
        let plugin_path = Self::plugin_path(scope, project_root)?;
        if !plugin_path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&plugin_path).unwrap_or_default();
        if raw.contains("Managed by AgentBox") {
            std::fs::remove_file(&plugin_path)?;
            Ok(Some(plugin_path))
        } else {
            Ok(None)
        }
    }

    fn is_installed(&self, scope: InstallScope, project_root: &Path) -> bool {
        let Ok(plugin_path) = Self::plugin_path(scope, project_root) else {
            return false;
        };
        std::fs::read_to_string(plugin_path)
            .map(|s| s.contains("Managed by AgentBox"))
            .unwrap_or(false)
    }

    fn parse_hook_event(&self, raw_json: &str) -> Result<NormalizedHookEvent> {
        let val: Value = serde_json::from_str(raw_json)
            .context("Invalid JSON received on OpenCode hook stdin")?;

        let event_name = val
            .get("hook_event_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let kind = match event_name {
            "SessionStart" => HookEventKind::SessionStart,
            "tool.execute.before" | "PreToolUse" => HookEventKind::PreToolUse,
            "tool.execute.after" | "PostToolUse" => HookEventKind::PostToolUse,
            "SessionEnd" => HookEventKind::SessionEnd,
            other => bail!("Unsupported OpenCode hook_event_name: {}", other),
        };

        let session_id = val
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("opencode-session")
            .to_string();
        let cwd = PathBuf::from(val.get("cwd").and_then(|v| v.as_str()).unwrap_or("."));
        let tool_name = val
            .get("tool_name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let tool_use_id = val
            .get("tool_use_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let tool_input = val.get("tool_input").cloned().unwrap_or(Value::Null);

        Ok(NormalizedHookEvent {
            agent: "opencode".to_string(),
            kind,
            session_id,
            cwd,
            transcript_path: None,
            tool_name,
            tool_use_id,
            tool_input,
            exit_status: Some(0),
            is_error: false,
        })
    }

    fn format_pretool_decision(&self, decision: &str, reason: &str) -> Option<String> {
        if decision == "allow" {
            return None;
        }
        serde_json::to_string(&json!({
            "decision": decision,
            "reason": reason
        }))
        .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opencode_plugin_install_and_parse() {
        let dir = std::env::temp_dir().join(format!("abox_oc_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let adapter = OpenCodeAdapter::new();
        let rep = adapter
            .install_hooks(InstallScope::Project, &dir, Path::new("/usr/local/bin/abox"))
            .unwrap();
        assert!(rep.config_path.exists());
        let js = std::fs::read_to_string(&rep.config_path).unwrap();
        assert!(js.contains("tool.execute.before") && js.contains("Managed by AgentBox"));
        assert!(adapter.is_installed(InstallScope::Project, &dir));

        let ev = adapter
            .parse_hook_event(
                r#"{"hook_event_name":"tool.execute.before","session_id":"oc-1","cwd":"/tmp","tool_name":"bash","tool_input":{"command":"pwd"}}"#,
            )
            .unwrap();
        assert_eq!(ev.kind, HookEventKind::PreToolUse);
        assert_eq!(ev.agent, "opencode");

        adapter.uninstall_hooks(InstallScope::Project, &dir).unwrap();
        assert!(!rep.config_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
