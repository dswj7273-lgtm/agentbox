use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn abox_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_abox"))
}

struct TestEnv {
    root: PathBuf,
    proj: PathBuf,
    abox_home: PathBuf,
    claude_home: PathBuf,
}

impl TestEnv {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("abox_e2e_{}_{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let proj = root.join("project");
        let abox_home = root.join("agentbox_home");
        let claude_home = root.join("claude_home");
        fs::create_dir_all(&proj).unwrap();
        fs::create_dir_all(&abox_home).unwrap();
        fs::create_dir_all(&claude_home).unwrap();

        // Initialize user's own git repo to verify zero-intrusion
        Command::new("git")
            .arg("-C")
            .arg(&proj)
            .arg("init")
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&proj)
            .args(["config", "user.name", "Dev"])
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&proj)
            .args(["config", "user.email", "dev@example.com"])
            .output()
            .unwrap();

        Self {
            root,
            proj,
            abox_home,
            claude_home,
        }
    }

    fn run_abox(&self, args: &[&str], stdin_data: Option<&str>) -> std::process::Output {
        let mut cmd = Command::new(abox_bin());
        cmd.current_dir(&self.proj)
            .env("AGENTBOX_HOME", &self.abox_home)
            .env("AGENTBOX_CLAUDE_HOME", &self.claude_home)
            .args(args);

        if stdin_data.is_some() {
            cmd.stdin(Stdio::piped());
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd.spawn().unwrap();
        if let Some(input) = stdin_data {
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
        }
        child.wait_with_output().unwrap()
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn e2e_init_bash_rm_50_files_failure_guard_and_undo() {
    let env = TestEnv::new("full_flow");

    // Seed project with 50 files in src/legacy + main.rs + ignored node_modules
    fs::create_dir_all(env.proj.join("src/legacy")).unwrap();
    fs::create_dir_all(env.proj.join("node_modules/pkg")).unwrap();
    fs::write(env.proj.join(".gitignore"), "node_modules/\n").unwrap();
    fs::write(env.proj.join("node_modules/pkg/index.js"), "ignored").unwrap();
    fs::write(env.proj.join("src/main.rs"), "fn main() {}\n").unwrap();

    for i in 0..50 {
        fs::write(
            env.proj.join(format!("src/legacy/file_{:02}.txt", i)),
            format!("legacy content #{}\n", i),
        )
        .unwrap();
    }

    Command::new("git")
        .arg("-C")
        .arg(&env.proj)
        .args(["add", "."])
        .output()
        .unwrap();
    Command::new("git")
        .arg("-C")
        .arg(&env.proj)
        .args(["commit", "-m", "initial"])
        .output()
        .unwrap();

    let git_head_before = String::from_utf8_lossy(
        &Command::new("git")
            .arg("-C")
            .arg(&env.proj)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .to_string();

    // 1. abox init --project
    let init_out = env.run_abox(&["init", "--project"], None);
    assert!(
        init_out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    assert!(env.proj.join(".claude/settings.json").exists());

    // 2. SessionStart hook
    let sess_start = serde_json::json!({
        "hook_event_name": "SessionStart",
        "session_id": "sess-e2e-1",
        "cwd": env.proj.to_string_lossy(),
    })
    .to_string();
    let out = env.run_abox(&["hook", "claude"], Some(&sess_start));
    assert!(out.status.success());

    // 3. Guard checks: force push -> ask, .env write -> deny
    let force_push = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "sess-e2e-1",
        "cwd": env.proj.to_string_lossy(),
        "tool_name": "Bash",
        "tool_use_id": "tu-fp",
        "tool_input": { "command": "git push --force origin main" }
    })
    .to_string();
    let out = env.run_abox(&["hook", "claude"], Some(&force_push));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"permissionDecision\":\"ask\""), "stdout={}", stdout);

    let env_write = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "sess-e2e-1",
        "cwd": env.proj.to_string_lossy(),
        "tool_name": "Write",
        "tool_use_id": "tu-env",
        "tool_input": { "file_path": env.proj.join(".env.local").to_string_lossy() }
    })
    .to_string();
    let out = env.run_abox(&["hook", "claude"], Some(&env_write));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"permissionDecision\":\"deny\""), "stdout={}", stdout);

    // 4. Bash rm -rf src/legacy (50 files deleted!) -> allowed + snapshot captured
    let pre_rm = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "sess-e2e-1",
        "cwd": env.proj.to_string_lossy(),
        "tool_name": "Bash",
        "tool_use_id": "tu-rm",
        "tool_input": { "command": "rm -rf src/legacy" }
    })
    .to_string();
    let out = env.run_abox(&["hook", "claude"], Some(&pre_rm));
    assert!(String::from_utf8_lossy(&out.stdout).trim().is_empty());

    fs::remove_dir_all(env.proj.join("src/legacy")).unwrap();

    let post_rm = serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": "sess-e2e-1",
        "cwd": env.proj.to_string_lossy(),
        "tool_name": "Bash",
        "tool_use_id": "tu-rm",
        "tool_input": { "command": "rm -rf src/legacy" },
        "tool_response": { "exitCode": 0 }
    })
    .to_string();
    let out = env.run_abox(&["hook", "claude"], Some(&post_rm));
    assert!(out.status.success());

    // 5. PostToolUseFailure: Bash writes src/broken.txt then exits 1
    let pre_fail = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "sess-e2e-1",
        "cwd": env.proj.to_string_lossy(),
        "tool_name": "Bash",
        "tool_use_id": "tu-fail",
        "tool_input": { "command": "echo broken > src/broken.txt && exit 1" }
    })
    .to_string();
    env.run_abox(&["hook", "claude"], Some(&pre_fail));
    fs::write(env.proj.join("src/broken.txt"), "broken\n").unwrap();
    let post_fail = serde_json::json!({
        "hook_event_name": "PostToolUseFailure",
        "session_id": "sess-e2e-1",
        "cwd": env.proj.to_string_lossy(),
        "tool_name": "Bash",
        "tool_use_id": "tu-fail",
        "tool_input": { "command": "echo broken > src/broken.txt && exit 1" },
        "error": "Exit code 1"
    })
    .to_string();
    env.run_abox(&["hook", "claude"], Some(&post_fail));

    // Check abox log output
    let log_out = env.run_abox(&["log"], None);
    let log_str = String::from_utf8_lossy(&log_out.stdout);
    assert!(log_str.contains("rm -rf src/legacy"), "log={}", log_str);
    assert!(log_str.contains("EXIT 1"), "log={}", log_str);

    // 6. abox undo --to <rm_step> --yes restores all 50 files byte-for-byte & deletes broken.txt!
    // Steps: #1 = denied .env Write, #2 = rm -rf src/legacy, #3 = failing bash (Wait: tu-fp was 'ask', never completed -> stayed inflight; denied .env was #1; rm was #2)
    let undo_out = env.run_abox(&["undo", "--to", "2", "--yes"], None);
    let undo_str = String::from_utf8_lossy(&undo_out.stdout);
    assert!(undo_out.status.success(), "undo stderr: {}", String::from_utf8_lossy(&undo_out.stderr));
    assert!(undo_str.contains("Restored"), "undo_str={}", undo_str);

    for i in 0..50 {
        let p = env.proj.join(format!("src/legacy/file_{:02}.txt", i));
        assert!(p.exists(), "Missing restored file {:?}", p);
        assert_eq!(
            fs::read_to_string(&p).unwrap(),
            format!("legacy content #{}\n", i)
        );
    }
    assert!(!env.proj.join("src/broken.txt").exists());
    assert!(env.proj.join("node_modules/pkg/index.js").exists());

    // 7. Revert the undo itself (`abox undo --yes`) -> src/legacy removed again
    let reundo = env.run_abox(&["undo", "--yes"], None);
    assert!(reundo.status.success());
    assert!(!env.proj.join("src/legacy/file_00.txt").exists());

    // Undo once more to bring files back, then test conflict detection on manual edits
    let reundo2 = env.run_abox(&["undo", "--yes"], None);
    assert!(reundo2.status.success());
    assert!(env.proj.join("src/legacy/file_00.txt").exists());

    fs::write(env.proj.join("src/main.rs"), "// manual edit\n").unwrap();
    let conflict = env.run_abox(&["undo", "--yes"], None);
    let conflict_str = String::from_utf8_lossy(&conflict.stdout);
    assert!(
        conflict_str.contains("modified outside AgentBox"),
        "expected conflict warning, got: {}",
        conflict_str
    );

    // Verify user's own git HEAD was never touched
    let git_head_after = String::from_utf8_lossy(
        &Command::new("git")
            .arg("-C")
            .arg(&env.proj)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .to_string();
    assert_eq!(git_head_before, git_head_after);

    // Uninstall cleanly
    let uninst = env.run_abox(&["uninstall", "--project"], None);
    assert!(uninst.status.success());
    let settings_after = fs::read_to_string(env.proj.join(".claude/settings.json")).unwrap();
    assert!(!settings_after.contains("hook claude"));
}

#[test]
fn e2e_diff_show_undo_only_policy_override_and_report() {
    let env = TestEnv::new("extra_features");
    fs::create_dir_all(env.proj.join("src")).unwrap();
    fs::create_dir_all(env.proj.join(".agentbox")).unwrap();
    fs::write(env.proj.join("src/a.txt"), "v1\n").unwrap();
    fs::write(env.proj.join("src/b.txt"), "v1\n").unwrap();
    // Custom project policy: block `echo forbidden`
    fs::write(
        env.proj.join(".agentbox/policy.yaml"),
        r#"rules:
  - id: custom.no_forbidden
    match:
      tool: Bash
      command: "forbidden"
    action: deny
"#,
    )
    .unwrap();

    env.run_abox(&["init", "--project"], None);

    // SessionStart
    let ss = serde_json::json!({
        "hook_event_name": "SessionStart",
        "session_id": "sess-extra",
        "cwd": env.proj.to_string_lossy()
    })
    .to_string();
    env.run_abox(&["hook", "claude"], Some(&ss));

    // Custom policy check
    let blocked = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "sess-extra",
        "cwd": env.proj.to_string_lossy(),
        "tool_name": "Bash",
        "tool_use_id": "tu-forbid",
        "tool_input": { "command": "echo forbidden" }
    })
    .to_string();
    let bout = env.run_abox(&["hook", "claude"], Some(&blocked));
    assert!(String::from_utf8_lossy(&bout.stdout).contains("deny"));

    // Mutate both a.txt and b.txt in one Bash step (#2)
    let pre = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "sess-extra",
        "cwd": env.proj.to_string_lossy(),
        "tool_name": "Bash",
        "tool_use_id": "tu-both",
        "tool_input": { "command": "edit both files" }
    })
    .to_string();
    env.run_abox(&["hook", "claude"], Some(&pre));
    fs::write(env.proj.join("src/a.txt"), "v2\n").unwrap();
    fs::write(env.proj.join("src/b.txt"), "v2\n").unwrap();
    let post = serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": "sess-extra",
        "cwd": env.proj.to_string_lossy(),
        "tool_name": "Bash",
        "tool_use_id": "tu-both",
        "tool_input": { "command": "edit both files" },
        "tool_response": { "exitCode": 0 }
    })
    .to_string();
    env.run_abox(&["hook", "claude"], Some(&post));

    // Simulate Stop hook with transcript_path to verify per-step & session cost backfill
    let transcript = env.root.join("sess-extra.jsonl");
    fs::write(
        &transcript,
        r#"{"type":"assistant","uuid":"u-ex","sessionId":"sess-extra","cwd":"/tmp","timestamp":"2026-11-17T02:00:00Z","message":{"id":"m-ex","model":"claude-sonnet-4-5","content":[{"type":"tool_use","id":"tu-both","name":"Bash","input":{"command":"edit both files"}}],"usage":{"input_tokens":10000,"output_tokens":2000,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
    )
    .unwrap();
    let stop_evt = serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": "sess-extra",
        "cwd": env.proj.to_string_lossy(),
        "transcript_path": transcript.to_string_lossy()
    })
    .to_string();
    env.run_abox(&["hook", "claude"], Some(&stop_evt));

    // Verify `abox log` displays backfilled per-step cost ($0.06)
    let log_cost = env.run_abox(&["log"], None);
    let log_cost_str = String::from_utf8_lossy(&log_cost.stdout);
    assert!(log_cost_str.contains("$0.06"), "expected $0.06 in log, got:\n{}", log_cost_str);

    // Verify `abox show 2` and `abox diff 2 --patch`
    let show_out = env.run_abox(&["show", "2"], None);
    let show_str = String::from_utf8_lossy(&show_out.stdout);
    assert!(show_str.contains("src/a.txt"), "show={}", show_str);
    assert!(show_str.contains("src/b.txt"), "show={}", show_str);

    let diff_out = env.run_abox(&["diff", "2", "--patch"], None);
    let diff_str = String::from_utf8_lossy(&diff_out.stdout);
    assert!(diff_str.contains("-v1") && diff_str.contains("+v2"), "diff={}", diff_str);

    // US2: Selective undo ONLY src/a.txt, keeping src/b.txt at v2
    let undo_only = env.run_abox(&["undo", "2", "--only", "src/a.txt", "--yes"], None);
    assert!(undo_only.status.success());
    assert_eq!(fs::read_to_string(env.proj.join("src/a.txt")).unwrap(), "v1\n");
    assert_eq!(fs::read_to_string(env.proj.join("src/b.txt")).unwrap(), "v2\n");

    // F6: abox report on synthetic Claude + Codex logs
    let logs_dir = env.claude_home.join("projects/myproj");
    fs::create_dir_all(&logs_dir).unwrap();
    fs::write(
        logs_dir.join("s1.jsonl"),
        r#"{"type":"assistant","uuid":"u1","sessionId":"s1","cwd":"/tmp/myproj","timestamp":"2026-11-17T01:00:00Z","message":{"id":"m1","model":"claude-sonnet-4-5","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"git push --force origin main"}}],"usage":{"input_tokens":500,"output_tokens":100,"cache_creation_input_tokens":0,"cache_read_input_tokens":1000}}}"#,
    )
    .unwrap();
    let codex_sessions = env.root.join("codex_rep_home/sessions");
    fs::create_dir_all(&codex_sessions).unwrap();
    fs::write(
        codex_sessions.join("rollout-1.jsonl"),
        r#"{"session_id":"cx-1","cwd":"/tmp/myproj","model":"gpt-5-codex","usage":{"input_tokens":400,"output_tokens":80,"cached_input_tokens":200}}"#,
    )
    .unwrap();
    let html_file = env.root.join("report.html");
    let mut rep_cmd = Command::new(abox_bin());
    let rep_out = rep_cmd
        .current_dir(&env.proj)
        .env("AGENTBOX_HOME", &env.abox_home)
        .env("AGENTBOX_CLAUDE_HOME", &env.claude_home)
        .env("AGENTBOX_CODEX_HOME", env.root.join("codex_rep_home"))
        .env("AGENTBOX_GEMINI_HOME", env.root.join("gemini_rep_home"))
        .args([
            "report",
            "--days",
            "30",
            "--json",
            "--html",
            html_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(rep_out.status.success());
    let rep_json = String::from_utf8_lossy(&rep_out.stdout);
    assert!(rep_json.contains("\"high_risk_commands\": 1"), "rep={}", rep_json);
    assert!(rep_json.contains("\"sessions_count\": 2"), "rep={}", rep_json);
    assert!(html_file.exists());

    // doctor
    let doc_out = env.run_abox(&["doctor"], None);
    assert!(doc_out.status.success());
    assert!(String::from_utf8_lossy(&doc_out.stdout).contains("Git:"));

    // policy init / list / test & log/show --json & gc
    let p_init = env.run_abox(&["policy", "init"], None);
    assert!(p_init.status.success());
    assert!(env.proj.join(".aboxignore").exists());

    let p_list = env.run_abox(&["policy", "list"], None);
    assert!(String::from_utf8_lossy(&p_list.stdout).contains("git.force_push"));

    let p_test = env.run_abox(&["policy", "test", "Bash", "git reset --hard"], None);
    assert!(String::from_utf8_lossy(&p_test.stdout).contains("Decision: ask"));

    let log_json = env.run_abox(&["log", "--all", "--json"], None);
    assert!(String::from_utf8_lossy(&log_json.stdout).trim_start().starts_with('['));

    let show_json = env.run_abox(&["show", "2", "--json"], None);
    assert!(String::from_utf8_lossy(&show_json.stdout).contains("\"files\""));

    let gc_out = env.run_abox(&["gc", "--days", "14", "--max-gb", "2"], None);
    assert!(gc_out.status.success());
    assert!(String::from_utf8_lossy(&gc_out.stdout).contains("GC complete"));
}

#[test]
fn e2e_edge_cases_large_file_nested_repo_and_fail_open() {
    let env = TestEnv::new("edge_cases");
    fs::create_dir_all(env.proj.join("src")).unwrap();
    fs::write(env.proj.join("src/normal.txt"), "before\n").unwrap();

    // 21 MB large file (should be auto-excluded from shadow git and guarded against rm)
    let big_file = env.proj.join("weights.bin");
    {
        let f = fs::File::create(&big_file).unwrap();
        f.set_len(21 * 1024 * 1024).unwrap();
    }

    // Nested sub-git repository
    let sub_repo = env.proj.join("vendor/sub");
    fs::create_dir_all(&sub_repo).unwrap();
    Command::new("git").arg("-C").arg(&sub_repo).arg("init").output().unwrap();
    fs::write(sub_repo.join("lib.rs"), "pub fn sub() {}\n").unwrap();

    env.run_abox(&["init", "--project"], None);

    // 1. Fail-open test: malformed JSON on hook stdin must exit 0 and write error.log
    let fail_open = env.run_abox(&["hook", "claude"], Some("{not-valid-json"));
    assert!(fail_open.status.success(), "hook must fail-open on bad stdin");
    assert!(env.abox_home.join("error.log").exists());

    // 2. Large file deletion triggers Guard `ask` because it cannot be restored by snapshot
    let rm_big = env.run_abox(
        &["hook", "claude"],
        Some(&serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s-edge",
            "cwd": env.proj.to_string_lossy(),
            "tool_name": "Bash",
            "tool_use_id": "tu-big",
            "tool_input": { "command": "rm -f weights.bin" }
        }).to_string()),
    );
    assert!(
        String::from_utf8_lossy(&rm_big.stdout).contains("\"permissionDecision\":\"ask\""),
        "expected Guard ask when removing >20MB excluded file, got: {}",
        String::from_utf8_lossy(&rm_big.stdout)
    );

    // 3. Normal edit followed by undo must preserve both weights.bin and nested vendor/sub/.git
    env.run_abox(
        &["hook", "claude"],
        Some(&serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s-edge",
            "cwd": env.proj.to_string_lossy(),
            "tool_name": "Edit",
            "tool_use_id": "tu-norm",
            "tool_input": { "file_path": "src/normal.txt" }
        }).to_string()),
    );
    fs::write(env.proj.join("src/normal.txt"), "after\n").unwrap();
    env.run_abox(
        &["hook", "claude"],
        Some(&serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "s-edge",
            "cwd": env.proj.to_string_lossy(),
            "tool_name": "Edit",
            "tool_use_id": "tu-norm",
            "tool_input": { "file_path": "src/normal.txt" }
        }).to_string()),
    );

    let undo = env.run_abox(&["undo", "--yes"], None);
    assert!(undo.status.success());
    assert_eq!(fs::read_to_string(env.proj.join("src/normal.txt")).unwrap(), "before\n");
    assert!(big_file.exists());
    assert_eq!(fs::metadata(&big_file).unwrap().len(), 21 * 1024 * 1024);
    assert!(sub_repo.join(".git").exists());
    assert_eq!(fs::read_to_string(sub_repo.join("lib.rs")).unwrap(), "pub fn sub() {}\n");

    // 4. Named checkpoint + --dry-run + --to-tag + status
    let cp_out = env.run_abox(&["checkpoint", "safe-point"], None);
    assert!(cp_out.status.success());
    assert!(String::from_utf8_lossy(&cp_out.stdout).contains("safe-point"));

    fs::write(env.proj.join("src/normal.txt"), "experimental\n").unwrap();
    let dry = env.run_abox(&["undo", "--to-tag", "safe-point", "--dry-run"], None);
    let dry_str = String::from_utf8_lossy(&dry.stdout);
    assert!(dry_str.contains("Dry-run:") && dry_str.contains("src/normal.txt"), "dry={}", dry_str);
    // File must remain untouched after --dry-run
    assert_eq!(fs::read_to_string(env.proj.join("src/normal.txt")).unwrap(), "experimental\n");

    let status_out = env.run_abox(&["status"], None);
    assert!(String::from_utf8_lossy(&status_out.stdout).contains("modified outside AgentBox"));

    let diff_cp = env.run_abox(&["diff", "cp:safe-point", "--patch"], None);
    assert!(String::from_utf8_lossy(&diff_cp.stdout).contains("+experimental"));

    let projs_out = env.run_abox(&["projects"], None);
    assert!(String::from_utf8_lossy(&projs_out.stdout).contains("HASH"));

    let restore_tag = env.run_abox(&["undo", "--to-tag", "safe-point", "--force", "--yes"], None);
    assert!(restore_tag.status.success());
    assert_eq!(fs::read_to_string(env.proj.join("src/normal.txt")).unwrap(), "before\n");
}

#[test]
fn e2e_multi_agent_unified_timeline_claude_codex_gemini() {
    let env = TestEnv::new("multi_agent");
    let codex_home = env.root.join("codex_home");
    let gemini_home = env.root.join("gemini_home");
    fs::create_dir_all(&codex_home).unwrap();
    fs::create_dir_all(&gemini_home).unwrap();
    fs::create_dir_all(env.proj.join("src")).unwrap();
    fs::write(env.proj.join("src/app.ts"), "export const v = 0;\n").unwrap();

    // Run `abox init --project` with simulated ~/.codex and ~/.gemini detected
    let mut init_cmd = Command::new(abox_bin());
    let init_out = init_cmd
        .current_dir(&env.proj)
        .env("AGENTBOX_HOME", &env.abox_home)
        .env("AGENTBOX_CLAUDE_HOME", &env.claude_home)
        .env("AGENTBOX_CODEX_HOME", &codex_home)
        .env("AGENTBOX_GEMINI_HOME", &gemini_home)
        .args(["init", "--project"])
        .output()
        .unwrap();
    assert!(init_out.status.success());
    assert!(env.proj.join(".claude/settings.json").exists());
    assert!(env.proj.join(".codex/hooks.json").exists());
    assert!(env.proj.join(".gemini/settings.json").exists());
    let gemini_cfg = fs::read_to_string(env.proj.join(".gemini/settings.json")).unwrap();
    assert!(gemini_cfg.contains("BeforeTool") && gemini_cfg.contains("AfterTool"));

    // 1. Claude edits src/app.ts
    env.run_abox(
        &["hook", "claude"],
        Some(&serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s-claude",
            "cwd": env.proj.to_string_lossy(),
            "tool_name": "Edit",
            "tool_use_id": "c1",
            "tool_input": { "file_path": "src/app.ts" }
        }).to_string()),
    );
    fs::write(env.proj.join("src/app.ts"), "export const v = 1;\n").unwrap();
    env.run_abox(
        &["hook", "claude"],
        Some(&serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "s-claude",
            "cwd": env.proj.to_string_lossy(),
            "tool_name": "Edit",
            "tool_use_id": "c1",
            "tool_input": { "file_path": "src/app.ts" }
        }).to_string()),
    );

    // 2. Codex edits src/app.ts via shell_command
    env.run_abox(
        &["hook", "codex"],
        Some(&serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s-codex",
            "cwd": env.proj.to_string_lossy(),
            "tool_name": "shell_command",
            "tool_use_id": "x1",
            "tool_input": { "command": "echo v2 > src/app.ts" }
        }).to_string()),
    );
    fs::write(env.proj.join("src/app.ts"), "export const v = 2;\n").unwrap();
    env.run_abox(
        &["hook", "codex"],
        Some(&serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "s-codex",
            "cwd": env.proj.to_string_lossy(),
            "tool_name": "shell_command",
            "tool_use_id": "x1",
            "tool_input": { "command": "echo v2 > src/app.ts" }
        }).to_string()),
    );

    // 3. Gemini checks Guard with BeforeTool on `git push --force` -> decision=ask
    let gem_guard = env.run_abox(
        &["hook", "gemini"],
        Some(&serde_json::json!({
            "hook_event_name": "BeforeTool",
            "session_id": "s-gemini",
            "cwd": env.proj.to_string_lossy(),
            "tool_name": "run_shell_command",
            "tool_use_id": "g1",
            "tool_input": { "command": "git push --force origin main" }
        }).to_string()),
    );
    let gem_out = String::from_utf8_lossy(&gem_guard.stdout);
    assert!(gem_out.contains("\"decision\":\"ask\""), "gem_out={}", gem_out);

    // Gemini writes src/gemini.ts via BeforeTool / AfterTool
    env.run_abox(
        &["hook", "gemini"],
        Some(&serde_json::json!({
            "hook_event_name": "BeforeTool",
            "session_id": "s-gemini",
            "cwd": env.proj.to_string_lossy(),
            "tool_name": "write_file",
            "tool_use_id": "g2",
            "tool_input": { "file_path": "src/gemini.ts" }
        }).to_string()),
    );
    fs::write(env.proj.join("src/gemini.ts"), "export const g = true;\n").unwrap();
    env.run_abox(
        &["hook", "gemini"],
        Some(&serde_json::json!({
            "hook_event_name": "AfterTool",
            "session_id": "s-gemini",
            "cwd": env.proj.to_string_lossy(),
            "tool_name": "write_file",
            "tool_use_id": "g2",
            "tool_input": { "file_path": "src/gemini.ts" }
        }).to_string()),
    );

    // Unified timeline across all 3 agents!
    let log_all = env.run_abox(&["log", "--all"], None);
    let log_str = String::from_utf8_lossy(&log_all.stdout);
    assert!(log_str.contains("claude"), "log={}", log_str);
    assert!(log_str.contains("codex"), "log={}", log_str);
    assert!(log_str.contains("gemini"), "log={}", log_str);

    // Cost summary by agent
    let cost_out = env.run_abox(&["cost", "--by", "agent"], None);
    let cost_str = String::from_utf8_lossy(&cost_out.stdout);
    assert!(cost_str.contains("claude") && cost_str.contains("codex") && cost_str.contains("gemini"), "cost={}", cost_str);

    // F10: Export JSONL and OpenTelemetry OTLP/JSON
    let exp_jsonl = env.run_abox(&["export", "--format", "jsonl", "--all"], None);
    assert!(exp_jsonl.status.success());
    let jsonl_str = String::from_utf8_lossy(&exp_jsonl.stdout);
    assert!(jsonl_str.contains("\"agent\":\"claude\"") && jsonl_str.contains("\"agent\":\"gemini\""));

    let exp_otel = env.run_abox(&["export", "--format", "otel", "--all"], None);
    assert!(exp_otel.status.success());
    let otel_str = String::from_utf8_lossy(&exp_otel.stdout);
    assert!(otel_str.contains("\"resourceSpans\"") && otel_str.contains("agentbox.audit"));

    let exp_md = env.run_abox(&["export", "--format", "markdown", "--all"], None);
    assert!(exp_md.status.success());
    let md_str = String::from_utf8_lossy(&exp_md.stdout);
    assert!(md_str.contains("### AgentBox Audit & Cost Summary") && md_str.contains("| `claude` |"));

    // 4. OpenCode plugin hook + Guard + PostToolUse snapshot
    let opencode_guard = env.run_abox(
        &["hook", "opencode"],
        Some(&serde_json::json!({
            "hook_event_name": "tool.execute.before",
            "session_id": "s-opencode",
            "cwd": env.proj.to_string_lossy(),
            "tool_name": "bash",
            "tool_use_id": "oc-1",
            "tool_input": { "command": "git push --force origin main" }
        }).to_string()),
    );
    assert!(String::from_utf8_lossy(&opencode_guard.stdout).contains("\"decision\":\"ask\""));

    env.run_abox(
        &["hook", "opencode"],
        Some(&serde_json::json!({
            "hook_event_name": "tool.execute.before",
            "session_id": "s-opencode",
            "cwd": env.proj.to_string_lossy(),
            "tool_name": "write",
            "tool_use_id": "oc-2",
            "tool_input": { "filePath": "src/opencode.ts" }
        }).to_string()),
    );
    fs::write(env.proj.join("src/opencode.ts"), "export const oc = 1;\n").unwrap();
    env.run_abox(
        &["hook", "opencode"],
        Some(&serde_json::json!({
            "hook_event_name": "tool.execute.after",
            "session_id": "s-opencode",
            "cwd": env.proj.to_string_lossy(),
            "tool_name": "write",
            "tool_use_id": "oc-2",
            "tool_input": { "filePath": "src/opencode.ts" }
        }).to_string()),
    );
    let log_oc = env.run_abox(&["log", "--all", "--agent", "opencode"], None);
    assert!(String::from_utf8_lossy(&log_oc.stdout).contains("src/opencode.ts"));

    // Explicit --agent opencode install check
    let init_oc = env.run_abox(&["init", "--project", "--agent", "opencode"], None);
    assert!(init_oc.status.success());
    assert!(env.proj.join(".opencode/plugins/agentbox.js").exists());

    // Uninstall removes all 3 cleanly
    let mut un_cmd = Command::new(abox_bin());
    let un_out = un_cmd
        .current_dir(&env.proj)
        .env("AGENTBOX_HOME", &env.abox_home)
        .env("AGENTBOX_CLAUDE_HOME", &env.claude_home)
        .env("AGENTBOX_CODEX_HOME", &codex_home)
        .env("AGENTBOX_GEMINI_HOME", &gemini_home)
        .args(["uninstall", "--project"])
        .output()
        .unwrap();
    assert!(un_out.status.success());
    assert!(!fs::read_to_string(env.proj.join(".codex/hooks.json")).unwrap().contains("hook codex"));
    assert!(!fs::read_to_string(env.proj.join(".gemini/settings.json")).unwrap().contains("hook gemini"));
}
