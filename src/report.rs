use crate::adapter::claude::{parse_claude_transcript, ClaudeAdapter};
use crate::guard::GuardEngine;
use anyhow::Result;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Serialize)]
pub struct SessionCostEntry {
    pub session_id: String,
    pub project: String,
    pub model: String,
    pub shell_commands: usize,
    pub high_risk_commands: usize,
    pub total_tokens: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Serialize)]
pub struct AuditReport {
    pub days: u64,
    pub files_scanned: usize,
    pub sessions_count: usize,
    pub total_shell_commands: usize,
    pub high_risk_commands: usize,
    pub unique_edited_files: usize,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_tokens: i64,
    pub total_cost_usd: f64,
    pub top_sessions: Vec<SessionCostEntry>,
    pub risky_examples: Vec<String>,
}

pub fn generate_report(
    days: u64,
    custom_dir: Option<&Path>,
    json_out: bool,
    html_path: Option<&Path>,
) -> Result<()> {
    let scan_root = if let Some(d) = custom_dir {
        d.to_path_buf()
    } else {
        ClaudeAdapter::claude_home()
            .map(|h| h.join("projects"))
            .unwrap_or_else(|| PathBuf::from(".claude/projects"))
    };

    if !scan_root.exists() {
        println!(
            "No Claude Code session logs found at {}. Run Claude Code first or pass --dir.",
            scan_root.display()
        );
        return Ok(());
    }

    let cutoff_ts = chrono::Utc::now().timestamp() - (days as i64 * 86400);
    let guard = GuardEngine::load(None, None);

    let mut jsonl_files = Vec::new();
    for entry in WalkDir::new(&scan_root).into_iter().flatten() {
        if entry.file_type().is_file()
            && entry.path().extension().and_then(|e| e.to_str()) == Some("jsonl")
        {
            if let Ok(meta) = entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    if let Ok(dur) = modified.duration_since(std::time::UNIX_EPOCH) {
                        if (dur.as_secs() as i64) < cutoff_ts {
                            continue;
                        }
                    }
                }
            }
            jsonl_files.push(entry.path().to_path_buf());
        }
    }

    let mut sessions_map: HashMap<String, SessionCostEntry> = HashMap::new();
    let mut edited_files_all: HashSet<String> = HashSet::new();
    let mut total_shell = 0usize;
    let mut total_risky = 0usize;
    let mut total_in = 0i64;
    let mut total_out = 0i64;
    let mut total_cache = 0i64;
    let mut total_cost = 0.0f64;
    let mut risky_examples: Vec<String> = Vec::new();

    for path in &jsonl_files {
        let Ok(m) = parse_claude_transcript(path) else {
            continue;
        };
        if let Some(ended) = m.ended_at {
            if ended < cutoff_ts {
                continue;
            }
        }

        let proj_path = m
            .project_cwd
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        let mut sess_risky = 0usize;
        for cmd in &m.shell_commands {
            if let Some(rule) = guard.is_high_risk_command(cmd, &proj_path) {
                sess_risky += 1;
                if risky_examples.len() < 5 {
                    let short: String = cmd.replace('\n', " ").chars().take(64).collect();
                    risky_examples.push(format!("[{}] {}", rule, short));
                }
            }
        }

        total_shell += m.shell_commands.len();
        total_risky += sess_risky;
        for f in &m.edited_files {
            edited_files_all.insert(f.clone());
        }

        let cache_toks = m.cache_write_tokens + m.cache_read_tokens;
        total_in += m.input_tokens;
        total_out += m.output_tokens;
        total_cache += cache_toks;
        total_cost += m.cost_usd;

        let proj_label = proj_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        let total_tokens = m.input_tokens + m.output_tokens + cache_toks;
        let entry = sessions_map
            .entry(m.session_id.clone())
            .or_insert_with(|| SessionCostEntry {
                session_id: m.session_id.clone(),
                project: proj_label,
                model: m
                    .primary_model
                    .clone()
                    .unwrap_or_else(|| "unknown".into()),
                shell_commands: 0,
                high_risk_commands: 0,
                total_tokens: 0,
                cost_usd: 0.0,
            });
        entry.shell_commands += m.shell_commands.len();
        entry.high_risk_commands += sess_risky;
        entry.total_tokens += total_tokens;
        entry.cost_usd += m.cost_usd;
    }

    let mut top_sessions: Vec<SessionCostEntry> = sessions_map.into_values().collect();
    let sessions_count = top_sessions.len();
    top_sessions.sort_by(|a, b| b.cost_usd.total_cmp(&a.cost_usd));
    top_sessions.truncate(5);

    let report = AuditReport {
        days,
        files_scanned: jsonl_files.len(),
        sessions_count,
        total_shell_commands: total_shell,
        high_risk_commands: total_risky,
        unique_edited_files: edited_files_all.len(),
        total_input_tokens: total_in,
        total_output_tokens: total_out,
        total_cache_tokens: total_cache,
        total_cost_usd: total_cost,
        top_sessions,
        risky_examples,
    };

    if json_out {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_terminal_card(&report);
    }

    if let Some(html_dest) = html_path {
        write_html_report(&report, html_dest)?;
        println!("\nHTML report saved -> {}", html_dest.display());
    }

    Ok(())
}

fn print_terminal_card(r: &AuditReport) {
    let total_tokens = r.total_input_tokens + r.total_output_tokens + r.total_cache_tokens;
    println!("╭─────────────────────────────────────────────────────────────────╮");
    println!(
        "│  AgentBox Audit Report (Last {:>2} days · Claude Code logs)       │",
        r.days
    );
    println!("├─────────────────────────────────────────────────────────────────┤");
    println!(
        "│  Sessions:       {:<10}   Est. API Cost:  ${:<12.2}   │",
        r.sessions_count, r.total_cost_usd
    );
    println!(
        "│  Shell Commands: {:<10}   High-Risk Cmds: {:<14}  │",
        r.total_shell_commands, r.high_risk_commands
    );
    println!(
        "│  Edited Files*:  {:<10}   Total Tokens:   {:<14}  │",
        r.unique_edited_files,
        format_tokens(total_tokens)
    );
    println!("├─────────────────────────────────────────────────────────────────┤");
    println!("│  Top 5 Most Expensive Sessions                                  │");
    if r.top_sessions.is_empty() {
        println!("│   (no sessions with token data found)                           │");
    } else {
        for (i, s) in r.top_sessions.iter().enumerate() {
            let sid: String = s.session_id.chars().take(8).collect();
            let proj: String = s.project.chars().take(18).collect();
            println!(
                "│  {}. {:<8}  {:<18}  {:>4} bash  {:>8}  ${:>6.2}  │",
                i + 1,
                sid,
                proj,
                s.shell_commands,
                format_tokens(s.total_tokens),
                s.cost_usd
            );
        }
    }
    if !r.risky_examples.is_empty() {
        println!("├─────────────────────────────────────────────────────────────────┤");
        println!("│  Sample High-Risk Shell Commands Detected                       │");
        for ex in &r.risky_examples {
            let trimmed: String = ex.chars().take(61).collect();
            println!("│  ⚠ {:<61}│", trimmed);
        }
    }
    println!("╰─────────────────────────────────────────────────────────────────╯");
    println!("* Edited Files counts Edit/Write/MultiEdit tool targets in historical logs;");
    println!("  run `abox init` to also snapshot & undo Bash-modified files step-by-step.");
}

fn write_html_report(r: &AuditReport, path: &Path) -> Result<()> {
    let rows: String = r
        .top_sessions
        .iter()
        .map(|s| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>${:.2}</td></tr>",
                html_escape(&s.session_id),
                html_escape(&s.project),
                html_escape(&s.model),
                s.shell_commands,
                s.cost_usd
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let html = format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>AgentBox Report</title>
<style>body{{font-family:system-ui,sans-serif;max-width:860px;margin:40px auto;padding:0 20px;background:#0f172a;color:#e2e8f0}}
.grid{{display:grid;grid-template-columns:repeat(3,1fr);gap:12px;margin:20px 0}}
.card{{background:#1e293b;padding:16px;border-radius:10px;border:1px solid #334155}}
.val{{font-size:28px;font-weight:700;color:#38bdf8}}
table{{width:100%;border-collapse:collapse;background:#1e293b;border-radius:10px;overflow:hidden}}
th,td{{padding:10px 12px;text-align:left;border-bottom:1px solid #334155}}</style></head>
<body><h1>AgentBox Audit Report (Last {} days)</h1>
<div class="grid">
<div class="card"><div>Sessions</div><div class="val">{}</div></div>
<div class="card"><div>Shell Commands (High-Risk)</div><div class="val">{} ({})</div></div>
<div class="card"><div>Est. API Cost</div><div class="val">${:.2}</div></div>
</div>
<h2>Top Sessions</h2>
<table><thead><tr><th>Session</th><th>Project</th><th>Model</th><th>Bash Cmds</th><th>Cost</th></tr></thead>
<tbody>{}</tbody></table></body></html>"#,
        r.days,
        r.sessions_count,
        r.total_shell_commands,
        r.high_risk_commands,
        r.total_cost_usd,
        rows
    );
    std::fs::write(path, html)?;
    Ok(())
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn format_tokens(t: i64) -> String {
    if t >= 1_000_000 {
        format!("{:.1}M", t as f64 / 1_000_000.0)
    } else if t >= 1_000 {
        format!("{:.1}K", t as f64 / 1_000.0)
    } else {
        t.to_string()
    }
}
