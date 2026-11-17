use globset::{Glob, GlobMatcher};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

impl Decision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Ask => "ask",
            Decision::Deny => "deny",
        }
    }

    fn priority(&self) -> u8 {
        match self {
            Decision::Allow => 0,
            Decision::Ask => 1,
            Decision::Deny => 2,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PolicyFile {
    #[serde(default)]
    pub rules: Vec<PolicyRuleConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PolicyRuleConfig {
    pub id: String,
    #[serde(rename = "match")]
    pub matcher: RuleMatchConfig,
    pub action: Decision,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StringOrVec {
    One(String),
    Many(Vec<String>),
}

impl StringOrVec {
    fn into_vec(self) -> Vec<String> {
        match self {
            StringOrVec::One(s) => vec![s],
            StringOrVec::Many(v) => v,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuleMatchConfig {
    #[serde(default)]
    pub tool: Option<StringOrVec>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub path: Option<StringOrVec>,
    #[serde(default)]
    pub path_scope: Option<String>, // "project" | "outside"
}

#[derive(Debug, Clone)]
struct CompiledRule {
    id: String,
    tools: Vec<String>,
    command_re: Option<Regex>,
    path_globs: Vec<GlobMatcher>,
    path_scope: Option<String>,
    action: Decision,
    reason: String,
}

#[derive(Debug, Clone)]
pub struct GuardVerdict {
    pub decision: Decision,
    pub rule_id: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RuleSummary {
    pub id: String,
    pub tools: Vec<String>,
    pub action: Decision,
    pub reason: String,
}

pub struct GuardEngine {
    rules: Vec<CompiledRule>,
}

impl GuardEngine {
    pub fn load(global_policy: Option<&Path>, project_policy: Option<&Path>) -> Self {
        let mut raw_rules = default_rules();

        for path in [global_policy, project_policy].into_iter().flatten() {
            if path.exists() {
                if let Ok(content) = std::fs::read_to_string(path) {
                    if let Ok(parsed) = serde_yaml::from_str::<PolicyFile>(&content) {
                        for r in parsed.rules {
                            // Override by rule id if already present, else append
                            if let Some(pos) = raw_rules.iter().position(|x| x.id == r.id) {
                                raw_rules[pos] = r;
                            } else {
                                raw_rules.push(r);
                            }
                        }
                    }
                }
            }
        }

        let compiled = raw_rules
            .into_iter()
            .filter_map(compile_rule)
            .collect();

        Self { rules: compiled }
    }

    pub fn list_rules(&self) -> Vec<RuleSummary> {
        self.rules
            .iter()
            .map(|r| RuleSummary {
                id: r.id.clone(),
                tools: r.tools.clone(),
                action: r.action,
                reason: r.reason.clone(),
            })
            .collect()
    }

    pub fn evaluate(
        &self,
        tool: &str,
        tool_input: &serde_json::Value,
        project_root: &Path,
        skipped_large_files: &[String],
    ) -> GuardVerdict {
        let mut best: Option<GuardVerdict> = None;

        let target_paths = extract_paths_from_input(tool, tool_input);
        let cmd_str = extract_command_from_input(tool, tool_input);

        // 1. Check skipped >20MB large files deletion/overwrite (cannot be recovered by snapshot!)
        if !skipped_large_files.is_empty() {
            if let Some(ref cmd) = cmd_str {
                for large in skipped_large_files {
                    if cmd.contains(large)
                        && (cmd.contains("rm ") || cmd.contains("mv ") || cmd.contains("> "))
                    {
                        return GuardVerdict {
                            decision: Decision::Ask,
                            rule_id: Some("snapshot.large_file_untracked".to_string()),
                            reason: Some(format!(
                                "`{}` exceeds 20MB and is excluded from AgentBox snapshots; modifying or deleting it cannot be undone.",
                                large
                            )),
                        };
                    }
                }
            }
        }

        // 2. Evaluate bash sub-commands recursively if Bash-like tool
        let sub_commands = cmd_str
            .as_deref()
            .map(split_shell_commands)
            .unwrap_or_default();

        for rule in &self.rules {
            if !rule.tools.is_empty() && !rule.tools.iter().any(|t| t == tool || t == "*") {
                continue;
            }

            let mut matched = false;

            if let Some(ref re) = rule.command_re {
                for sub in &sub_commands {
                    if re.is_match(sub) {
                        if let Some(ref scope) = rule.path_scope {
                            let is_outside = command_targets_outside_project(sub, project_root);
                            if (scope == "outside" && is_outside)
                                || (scope == "project" && !is_outside)
                            {
                                matched = true;
                                break;
                            }
                        } else {
                            matched = true;
                            break;
                        }
                    }
                }
            } else if !rule.path_globs.is_empty() {
                for p in &target_paths {
                    let expanded = expand_tilde(p);
                    if rule
                        .path_globs
                        .iter()
                        .any(|g| g.is_match(&expanded) || g.is_match(p))
                    {
                        matched = true;
                        break;
                    }
                }
                // Also inspect bash command tokens for sensitive paths when rule includes Bash
                if !matched {
                    for sub in &sub_commands {
                        for tok in sub.split_whitespace() {
                            let expanded = expand_tilde(tok);
                            if rule
                                .path_globs
                                .iter()
                                .any(|g| g.is_match(&expanded) || g.is_match(tok))
                            {
                                matched = true;
                                break;
                            }
                        }
                        if matched {
                            break;
                        }
                    }
                }
            }

            if matched {
                let candidate = GuardVerdict {
                    decision: rule.action,
                    rule_id: Some(rule.id.clone()),
                    reason: Some(rule.reason.clone()),
                };
                match &best {
                    None => best = Some(candidate),
                    Some(prev) if candidate.decision.priority() > prev.decision.priority() => {
                        best = Some(candidate);
                    }
                    Some(prev)
                        if candidate.decision == Decision::Allow
                            && prev.decision == Decision::Allow =>
                    {
                        best = Some(candidate);
                    }
                    _ => {}
                }
            }
        }

        // Special hardcoded safety net for `rm` outside project if not explicitly allowed
        if let Some(verdict) = best {
            return verdict;
        }

        GuardVerdict {
            decision: Decision::Allow,
            rule_id: None,
            reason: None,
        }
    }

    pub fn is_high_risk_command(&self, cmd: &str, project_root: &Path) -> Option<String> {
        let input = serde_json::json!({ "command": cmd });
        let verdict = self.evaluate("Bash", &input, project_root, &[]);
        if verdict.decision != Decision::Allow {
            verdict.rule_id
        } else {
            None
        }
    }
}

fn compile_rule(r: PolicyRuleConfig) -> Option<CompiledRule> {
    let tools = r.matcher.tool.map(|t| t.into_vec()).unwrap_or_default();
    let command_re = match r.matcher.command {
        Some(pat) => Regex::new(&pat).ok(),
        None => None,
    };
    let mut path_globs = Vec::new();
    if let Some(paths) = r.matcher.path {
        for p in paths.into_vec() {
            let expanded = expand_tilde(&p);
            if let Ok(g) = Glob::new(&expanded) {
                path_globs.push(g.compile_matcher());
            }
            if expanded != p {
                if let Ok(g) = Glob::new(&p) {
                    path_globs.push(g.compile_matcher());
                }
            }
        }
    }
    let reason = r.reason.unwrap_or_else(|| {
        format!(
            "Matched AgentBox rule `{}` (irreversible or sensitive target).",
            r.id
        )
    });
    Some(CompiledRule {
        id: r.id,
        tools,
        command_re,
        path_globs,
        path_scope: r.matcher.path_scope,
        action: r.action,
        reason,
    })
}

fn default_rules() -> Vec<PolicyRuleConfig> {
    vec![
        PolicyRuleConfig {
            id: "rm.outside_project".into(),
            matcher: RuleMatchConfig {
                tool: Some(StringOrVec::Many(vec![
                    "Bash".into(),
                    "run_shell_command".into(),
                    "shell_command".into(),
                    "exec_command".into(),
                ])),
                command: Some(r"(?i)(^|\s)rm\s+(-[a-z]*[rR][a-z]*|--recursive)".into()),
                path: None,
                path_scope: Some("outside".into()),
            },
            action: Decision::Ask,
            reason: Some(
                "Recursive `rm` targets a path outside the project directory and cannot be recovered by AgentBox snapshots."
                    .into(),
            ),
        },
        PolicyRuleConfig {
            id: "rm.in_project".into(),
            matcher: RuleMatchConfig {
                tool: Some(StringOrVec::Many(vec![
                    "Bash".into(),
                    "run_shell_command".into(),
                    "shell_command".into(),
                    "exec_command".into(),
                ])),
                command: Some(r"(?i)(^|\s)rm\s+(-[a-z]*[rR][a-z]*|--recursive)".into()),
                path: None,
                path_scope: Some("project".into()),
            },
            action: Decision::Allow,
            reason: Some("Covered by AgentBox filesystem snapshots.".into()),
        },
        PolicyRuleConfig {
            id: "git.force_push".into(),
            matcher: RuleMatchConfig {
                tool: Some(StringOrVec::Many(vec![
                    "Bash".into(),
                    "run_shell_command".into(),
                    "shell_command".into(),
                    "exec_command".into(),
                ])),
                command: Some(r"\bgit\s+push\b.*(\s-f\b|\s--force\b|\s--force-with-lease\b)".into()),
                path: None,
                path_scope: None,
            },
            action: Decision::Ask,
            reason: Some(
                "`git push --force` alters remote history and is irreversible via local snapshots."
                    .into(),
            ),
        },
        PolicyRuleConfig {
            id: "git.destructive_reset".into(),
            matcher: RuleMatchConfig {
                tool: Some(StringOrVec::Many(vec![
                    "Bash".into(),
                    "run_shell_command".into(),
                    "shell_command".into(),
                    "exec_command".into(),
                ])),
                command: Some(
                    r"\bgit\s+(reset\s+--hard|clean\s+-[a-z]*[fd][a-z]*|checkout\s+--\s+\.|restore\s+\.)"
                        .into(),
                ),
                path: None,
                path_scope: None,
            },
            action: Decision::Ask,
            reason: Some(
                "Destructive git command may wipe uncommitted work or untracked files.".into(),
            ),
        },
        PolicyRuleConfig {
            id: "db.destructive".into(),
            matcher: RuleMatchConfig {
                tool: Some(StringOrVec::Many(vec![
                    "Bash".into(),
                    "run_shell_command".into(),
                    "shell_command".into(),
                    "exec_command".into(),
                ])),
                command: Some(r"(?i)\b(DROP\s+(TABLE|DATABASE|SCHEMA)|TRUNCATE\s+TABLE)\b".into()),
                path: None,
                path_scope: None,
            },
            action: Decision::Ask,
            reason: Some(
                "Destructive database statement is an external side effect not covered by local snapshots."
                    .into(),
            ),
        },
        PolicyRuleConfig {
            id: "shell.pipe_exec".into(),
            matcher: RuleMatchConfig {
                tool: Some(StringOrVec::Many(vec![
                    "Bash".into(),
                    "run_shell_command".into(),
                    "shell_command".into(),
                    "exec_command".into(),
                ])),
                command: Some(r"(?i)\b(curl|wget)\b.*\|\s*(sh|bash|zsh)\b".into()),
                path: None,
                path_scope: None,
            },
            action: Decision::Ask,
            reason: Some("Piping remote script directly into a shell.".into()),
        },
        PolicyRuleConfig {
            id: "secrets.write".into(),
            matcher: RuleMatchConfig {
                tool: Some(StringOrVec::Many(vec![
                    "Edit".into(),
                    "Write".into(),
                    "MultiEdit".into(),
                    "NotebookEdit".into(),
                    "write_file".into(),
                    "edit_file".into(),
                ])),
                command: None,
                path: Some(StringOrVec::Many(vec![
                    "**/.env".into(),
                    "**/.env.*".into(),
                    "**/.ssh/**".into(),
                    "**/.aws/**".into(),
                    "**/.zshrc".into(),
                    "**/.bashrc".into(),
                    "**/.bash_profile".into(),
                    "**/.github/workflows/**".into(),
                ])),
                path_scope: None,
            },
            action: Decision::Deny,
            reason: Some(
                "Writing to `.env*`, SSH/AWS credentials, shell RC, or CI workflows is blocked by policy."
                    .into(),
            ),
        },
    ]
}

fn expand_tilde(path: &str) -> String {
    if path == "~" || path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            return path.replacen('~', &home.to_string_lossy(), 1);
        }
    }
    path.to_string()
}

fn extract_command_from_input(tool: &str, input: &serde_json::Value) -> Option<String> {
    if matches!(
        tool,
        "Bash" | "run_shell_command" | "shell_command" | "exec_command"
    ) {
        return input
            .get("command")
            .or_else(|| input.get("cmd"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
    }
    None
}

fn extract_paths_from_input(_tool: &str, input: &serde_json::Value) -> Vec<String> {
    let mut paths = Vec::new();
    for key in ["file_path", "path", "target_file", "notebook_path"] {
        if let Some(s) = input.get(key).and_then(|v| v.as_str()) {
            paths.push(s.to_string());
        }
    }
    paths
}

/// Split compound shell commands (`;`, `&&`, `||`, `|`, `\n`) respecting quotes,
/// strip wrappers (`sudo`, `env`, `command`), and unwrap `bash -c "..."` up to depth 3.
pub fn split_shell_commands(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    split_recursive(cmd, 0, &mut out);
    out
}

fn split_recursive(cmd: &str, depth: usize, out: &mut Vec<String>) {
    if depth > 3 {
        return;
    }

    // Check full command first so pipe rules like `curl ... | sh` match
    let trimmed = cmd.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
    }

    let mut segments = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if escape {
            current.push(c);
            escape = false;
            i += 1;
            continue;
        }
        if c == '\\' && !in_single {
            escape = true;
            current.push(c);
            i += 1;
            continue;
        }
        if c == '\'' && !in_double {
            in_single = !in_single;
            current.push(c);
            i += 1;
            continue;
        }
        if c == '"' && !in_single {
            in_double = !in_double;
            current.push(c);
            i += 1;
            continue;
        }
        if !in_single && !in_double {
            if c == ';' || c == '\n' {
                segments.push(current.trim().to_string());
                current.clear();
                i += 1;
                continue;
            }
            if (c == '&' || c == '|') && i + 1 < chars.len() && chars[i + 1] == c {
                segments.push(current.trim().to_string());
                current.clear();
                i += 2;
                continue;
            }
            if c == '|' {
                segments.push(current.trim().to_string());
                current.clear();
                i += 1;
                continue;
            }
        }
        current.push(c);
        i += 1;
    }
    if !current.trim().is_empty() {
        segments.push(current.trim().to_string());
    }

    for seg in segments {
        if seg.is_empty() {
            continue;
        }
        let normalized = strip_command_prefixes(&seg);
        if !normalized.is_empty() && normalized != trimmed {
            out.push(normalized.clone());
        }
        if let Some(inner) = unwrap_subshell_c(&normalized) {
            split_recursive(&inner, depth + 1, out);
        }
    }
}

fn strip_command_prefixes(seg: &str) -> String {
    let mut tokens: Vec<&str> = seg.split_whitespace().collect();
    while !tokens.is_empty() {
        let head = tokens[0];
        if head == "sudo" || head == "command" || head == "nohup" {
            tokens.remove(0);
            // Skip sudo flags with no args like -E, -H, -n
            while !tokens.is_empty() && tokens[0].starts_with('-') {
                tokens.remove(0);
            }
        } else if head == "env" {
            tokens.remove(0);
            while !tokens.is_empty()
                && (tokens[0].contains('=') || tokens[0].starts_with('-'))
            {
                tokens.remove(0);
            }
        } else {
            break;
        }
    }
    tokens.join(" ")
}

fn unwrap_subshell_c(cmd: &str) -> Option<String> {
    let re = Regex::new(r#"^(?:/bin/|/usr/bin/)?(?:ba|z|da)?sh\s+-[a-zA-Z]*c\s+(.*)$"#).ok()?;
    let caps = re.captures(cmd.trim())?;
    let body = caps.get(1)?.as_str().trim();
    if (body.starts_with('\'') && body.ends_with('\'') && body.len() >= 2)
        || (body.starts_with('"') && body.ends_with('"') && body.len() >= 2)
    {
        return Some(body[1..body.len() - 1].to_string());
    }
    Some(body.to_string())
}

fn command_targets_outside_project(sub_cmd: &str, project_root: &Path) -> bool {
    let tokens: Vec<&str> = sub_cmd.split_whitespace().collect();
    for tok in tokens.iter().skip(1) {
        if tok.starts_with('-') {
            continue;
        }
        let cleaned = tok.trim_matches(|c| c == '"' || c == '\'');
        if cleaned == "/"
            || cleaned == "~"
            || cleaned.starts_with("~/")
            || cleaned.starts_with("$HOME")
            || cleaned.starts_with("${HOME}")
            || cleaned == ".."
            || cleaned.starts_with("../")
            || cleaned.contains("/../")
        {
            return true;
        }
        if cleaned.starts_with('/') {
            let p = Path::new(cleaned);
            if !p.starts_with(project_root) {
                return true;
            }
        }
    }
    false
}

pub fn default_policy_template() -> &'static str {
    r#"# AgentBox Guard Policy (.agentbox/policy.yaml)
# Actions: allow | ask | deny
# Matching rule IDs override built-in rules; new IDs are appended.
rules:
  - id: git.force_push
    match:
      tool: [Bash, run_shell_command, shell_command, exec_command]
      command: '\bgit\s+push\b.*(\s-f\b|\s--force\b|\s--force-with-lease\b)'
    action: ask
    reason: "`git push --force` alters remote history and is irreversible via local snapshots."

  - id: secrets.write
    match:
      tool: [Edit, Write, MultiEdit, NotebookEdit, write_file, edit_file]
      path:
        - "**/.env"
        - "**/.env.*"
        - "**/.ssh/**"
        - "**/.aws/**"
    action: deny
    reason: "Writing to secret files is blocked by project policy."
"#
}

/// Redact common API keys / tokens from JSON before storing in SQLite (§9 Privacy).
pub fn redact_sensitive_text(input: &str) -> String {
    let patterns = [
        (r"sk-[A-Za-z0-9_-]{16,}", "[REDACTED_KEY]"),
        (r"gh[pousr]_[A-Za-z0-9_]{16,}", "[REDACTED_GH_TOKEN]"),
        (r"AKIA[0-9A-Z]{16}", "[REDACTED_AWS_KEY]"),
        (r"(?i)(Bearer\s+)[A-Za-z0-9._-]{16,}", "${1}[REDACTED_TOKEN]"),
    ];
    let mut out = input.to_string();
    for (pat, rep) in patterns {
        if let Ok(re) = Regex::new(pat) {
            out = re.replace_all(&out, rep).to_string();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_allows_rm_inside_project_and_asks_outside() {
        let engine = GuardEngine::load(None, None);
        let root = Path::new("/tmp/myproj");

        let v_in = engine.evaluate(
            "Bash",
            &serde_json::json!({"command": "rm -rf src/legacy"}),
            root,
            &[],
        );
        assert_eq!(v_in.decision, Decision::Allow);
        assert_eq!(v_in.rule_id.as_deref(), Some("rm.in_project"));

        let v_out = engine.evaluate(
            "Bash",
            &serde_json::json!({"command": "sudo env FOO=1 bash -c 'rm -rf ~/.ssh'"}),
            root,
            &[],
        );
        assert_eq!(v_out.decision, Decision::Ask);
        assert_eq!(v_out.rule_id.as_deref(), Some("rm.outside_project"));
    }

    #[test]
    fn guard_catches_force_push_and_env_write() {
        let engine = GuardEngine::load(None, None);
        let root = Path::new("/tmp/myproj");

        let v_push = engine.evaluate(
            "Bash",
            &serde_json::json!({"command": "git status && git push --force origin main"}),
            root,
            &[],
        );
        assert_eq!(v_push.decision, Decision::Ask);
        assert_eq!(v_push.rule_id.as_deref(), Some("git.force_push"));

        let v_env = engine.evaluate(
            "Write",
            &serde_json::json!({"file_path": "/tmp/myproj/.env.local"}),
            root,
            &[],
        );
        assert_eq!(v_env.decision, Decision::Deny);
        assert_eq!(v_env.rule_id.as_deref(), Some("secrets.write"));
    }

    #[test]
    fn redact_secrets_works() {
        let raw = r#"{"command":"curl -H 'Authorization: Bearer supersecret_token_123456' https://x"}"#;
        let red = redact_sensitive_text(raw);
        assert!(!red.contains("supersecret_token_123456"));
        assert!(red.contains("[REDACTED_TOKEN]"));
    }
}
