use crate::adapter::claude::{parse_claude_transcript, ClaudeAdapter};
use crate::adapter::codex::{parse_codex_transcript, CodexAdapter};
use crate::adapter::gemini::{parse_gemini_transcript, GeminiAdapter};
use crate::adapter::TranscriptMetrics;
use crate::guard::GuardEngine;
use anyhow::Result;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Serialize)]
pub struct SessionCostEntry {
    pub session_id: String,
    pub agent: String,
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
    pub agents_found: Vec<String>,
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
    project_filter: Option<&Path>,
    json_out: bool,
    html_path: Option<&Path>,
) -> Result<()> {
    let canonical_filter = project_filter.map(|p| {
        std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .to_string()
    });
    let cutoff_ts = chrono::Utc::now().timestamp() - (days as i64 * 86400);
    let mut sources: Vec<(&'static str, PathBuf)> = Vec::new();

    if let Some(d) = custom_dir {
        sources.push(("claude", d.to_path_buf()));
    } else {
        if let Some(ch) = ClaudeAdapter::claude_home() {
            let p = ch.join("projects");
            if p.exists() {
                sources.push(("claude", p));
            }
        }
        if let Some(xh) = CodexAdapter::codex_home() {
            for sub in ["sessions", "rollouts"] {
                let p = xh.join(sub);
                if p.exists() {
                    sources.push(("codex", p));
                }
            }
        }
        if let Some(gh) = GeminiAdapter::gemini_home() {
            let p = gh.join("tmp");
            if p.exists() {
                sources.push(("gemini", p));
            }
        }
    }

    if sources.is_empty() {
        println!(
            "No local agent session logs found (~/.claude/projects, ~/.codex/sessions, ~/.gemini/tmp). Pass --dir to specify a log directory."
        );
        return Ok(());
    }

    let guard = GuardEngine::load(None, None);
    let mut log_files: Vec<(&'static str, PathBuf)> = Vec::new();

    for (agent_kind, scan_root) in &sources {
        for entry in WalkDir::new(scan_root).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let ext = entry.path().extension().and_then(|e| e.to_str());
            if ext != Some("jsonl") {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    if let Ok(dur) = modified.duration_since(std::time::UNIX_EPOCH) {
                        if (dur.as_secs() as i64) < cutoff_ts {
                            continue;
                        }
                    }
                }
            }
            log_files.push((*agent_kind, entry.path().to_path_buf()));
        }
    }

    let mut sessions_map: HashMap<String, SessionCostEntry> = HashMap::new();
    let mut edited_files_all: HashSet<String> = HashSet::new();
    let mut agents_set: HashSet<String> = HashSet::new();
    let mut total_shell = 0usize;
    let mut total_risky = 0usize;
    let mut total_in = 0i64;
    let mut total_out = 0i64;
    let mut total_cache = 0i64;
    let mut total_cost = 0.0f64;
    let mut risky_examples: Vec<String> = Vec::new();

    for (agent_kind, path) in &log_files {
        let parsed: Result<TranscriptMetrics> = match *agent_kind {
            "codex" => parse_codex_transcript(path),
            "gemini" => parse_gemini_transcript(path),
            _ => parse_claude_transcript(path),
        };
        let Ok(m) = parsed else {
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

        if let Some(ref pf) = canonical_filter {
            let cwd_str = proj_path.to_string_lossy();
            if !cwd_str.starts_with(pf.as_str()) {
                continue;
            }
        }
        agents_set.insert((*agent_kind).to_string());

        let mut sess_risky = 0usize;
        for cmd in &m.shell_commands {
            // Check sub-commands individually so compound commands highlight the exact risky segment
            let subs = crate::guard::split_shell_commands(cmd);
            let mut matched_rule: Option<(String, String)> = None;
            for sub in subs.iter().rev() {
                if let Some(rule) = guard.is_high_risk_command(sub, &proj_path) {
                    matched_rule = Some((rule, sub.clone()));
                    break;
                }
            }
            if matched_rule.is_none() {
                if let Some(rule) = guard.is_high_risk_command(cmd, &proj_path) {
                    matched_rule = Some((rule, cmd.clone()));
                }
            }
            if let Some((rule, snippet)) = matched_rule {
                sess_risky += 1;
                if risky_examples.len() < 5 {
                    let clean: String = snippet
                        .replace('\n', " ")
                        .chars()
                        .filter(|c| c.is_ascii())
                        .take(58)
                        .collect();
                    risky_examples.push(format!("[{}] {}", rule, clean));
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
            .entry(format!("{}:{}", agent_kind, m.session_id))
            .or_insert_with(|| SessionCostEntry {
                session_id: m.session_id.clone(),
                agent: (*agent_kind).to_string(),
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

    let mut agents_found: Vec<String> = agents_set.into_iter().collect();
    agents_found.sort();

    let report = AuditReport {
        days,
        files_scanned: log_files.len(),
        sessions_count,
        agents_found,
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

fn print_box_line(content: &str) {
    const INNER_WIDTH: usize = 65;
    let char_len = content.chars().count();
    if char_len >= INNER_WIDTH {
        let truncated: String = content.chars().take(INNER_WIDTH).collect();
        println!("│{}│", truncated);
    } else {
        println!("│{}{}│", content, " ".repeat(INNER_WIDTH - char_len));
    }
}

fn print_terminal_card(r: &AuditReport) {
    let total_tokens = r.total_input_tokens + r.total_output_tokens + r.total_cache_tokens;
    let agents_str = if r.agents_found.is_empty() {
        "none".to_string()
    } else {
        r.agents_found.join(", ")
    };
    println!("╭─────────────────────────────────────────────────────────────────╮");
    print_box_line(&format!(
        "  AgentBox Audit Report (Last {} days · {})",
        r.days,
        agents_str.chars().take(24).collect::<String>()
    ));
    println!("├─────────────────────────────────────────────────────────────────┤");
    print_box_line(&format!(
        "  Sessions:       {:<10}   Est. API Equiv: ${:.2}",
        r.sessions_count, r.total_cost_usd
    ));
    print_box_line(&format!(
        "  Shell Commands: {:<10}   High-Risk Cmds: {}",
        r.total_shell_commands, r.high_risk_commands
    ));
    print_box_line(&format!(
        "  Edited Files*:  {:<10}   Total Tokens:   {}",
        r.unique_edited_files,
        format_tokens(total_tokens)
    ));
    println!("├─────────────────────────────────────────────────────────────────┤");
    print_box_line("  Top 5 Most Expensive Sessions");
    if r.top_sessions.is_empty() {
        print_box_line("   (no sessions with token data found)");
    } else {
        for (i, s) in r.top_sessions.iter().enumerate() {
            let sid: String = s.session_id.chars().take(8).collect();
            let proj: String = s.project.chars().take(12).collect();
            print_box_line(&format!(
                "  {}. {:<6} {:<8} {:<12} {:>4} bash {:>7} ${:>7.2}",
                i + 1,
                s.agent,
                sid,
                proj,
                s.shell_commands,
                format_tokens(s.total_tokens),
                s.cost_usd
            ));
        }
    }
    if !r.risky_examples.is_empty() {
        println!("├─────────────────────────────────────────────────────────────────┤");
        print_box_line("  Sample High-Risk Shell Commands Detected");
        for ex in &r.risky_examples {
            let trimmed: String = ex.chars().take(60).collect();
            print_box_line(&format!("  ! {}", trimmed));
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
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>${:.2}</td></tr>",
                html_escape(&s.agent),
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
<div class="card"><div>Est. API Equiv.</div><div class="val">${:.2}</div></div>
</div>
<h2>Top Sessions</h2>
<table><thead><tr><th>Agent</th><th>Session</th><th>Project</th><th>Model</th><th>Bash Cmds</th><th>Cost</th></tr></thead>
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
