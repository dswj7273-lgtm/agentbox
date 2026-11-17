use crate::db::{Database, LogFilter, StepRecord};
use crate::paths::{resolve_project_root, AgentBoxPaths};
use anyhow::{bail, Result};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;

pub fn cmd_export(
    format: &str,
    all_sessions: bool,
    agent: Option<String>,
    limit: usize,
    out_path: Option<&Path>,
) -> Result<()> {
    let paths = AgentBoxPaths::resolve()?;
    if !paths.db_path().exists() {
        bail!("No AgentBox database found. Run `abox init` first.");
    }
    let db = Database::open(&paths.db_path())?;
    let cwd = std::env::current_dir()?;
    let project_root = resolve_project_root(&cwd);

    let filter = LogFilter {
        project: Some(project_root.to_string_lossy().to_string()),
        all_sessions,
        agent,
        since_ts: None,
        file_path: None,
        limit,
    };
    let steps = db.query_steps(&filter)?;

    let payload = match format {
        "jsonl" => export_jsonl(&db, &steps)?,
        "otel" => export_otel(&db, &steps, &project_root.to_string_lossy())?,
        "markdown" => export_markdown(&db, &steps, &project_root.to_string_lossy())?,
        other => bail!(
            "Unsupported export format `{}` (expected jsonl, otel, or markdown)",
            other
        ),
    };

    if let Some(dest) = out_path {
        std::fs::write(dest, payload)?;
        println!("✔ Exported {} step(s) as {} -> {}", steps.len(), format, dest.display());
    } else {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(payload.as_bytes())?;
    }
    Ok(())
}

fn export_jsonl(db: &Database, steps: &[StepRecord]) -> Result<String> {
    let mut out = String::new();
    for s in steps {
        let files = db.get_step_files(s.id).unwrap_or_default();
        let line = json!({
            "step_id": s.id,
            "session_id": s.session_id,
            "seq": s.seq,
            "agent": s.agent,
            "tool": s.tool,
            "summary": s.input_summary,
            "files_changed": s.files_changed,
            "files": files.iter().map(|f| json!({"path": f.path, "change": f.change})).collect::<Vec<_>>(),
            "guard_decision": s.guard_decision,
            "guard_rule": s.guard_rule,
            "exit_status": s.exit_status,
            "duration_ms": s.duration_ms,
            "started_at": s.started_at,
            "pre_tree": s.pre_tree,
            "post_tree": s.post_tree,
            "is_undo": s.is_undo,
            "concurrent_overlap": s.concurrent_overlap,
        });
        out.push_str(&serde_json::to_string(&line)?);
        out.push('\n');
    }
    Ok(out)
}

/// Export steps as OpenTelemetry OTLP/JSON `ExportTraceServiceRequest` spans.
fn export_otel(db: &Database, steps: &[StepRecord], project: &str) -> Result<String> {
    let mut spans = Vec::with_capacity(steps.len());
    for s in steps {
        let trace_id = hex_hash(&s.session_id, 32);
        let span_id = hex_hash(&format!("{}-{}", s.session_id, s.id), 16);
        let start_ns = (s.started_at.max(0) as u64) * 1_000_000_000;
        let dur_ns = (s.duration_ms.unwrap_or(1).max(1) as u64) * 1_000_000;
        let end_ns = start_ns + dur_ns;
        let files = db.get_step_files(s.id).unwrap_or_default();
        let file_list: Vec<String> = files.into_iter().map(|f| f.path).collect();

        spans.push(json!({
            "traceId": trace_id,
            "spanId": span_id,
            "name": format!("agentbox.{}.{}", s.agent, s.tool),
            "kind": 1,
            "startTimeUnixNano": start_ns.to_string(),
            "endTimeUnixNano": end_ns.to_string(),
            "attributes": [
                {"key": "agentbox.step_id", "value": {"intValue": s.id.to_string()}},
                {"key": "agentbox.session_id", "value": {"stringValue": s.session_id}},
                {"key": "agentbox.agent", "value": {"stringValue": s.agent}},
                {"key": "agentbox.tool", "value": {"stringValue": s.tool}},
                {"key": "agentbox.summary", "value": {"stringValue": s.input_summary}},
                {"key": "agentbox.files_changed", "value": {"intValue": s.files_changed.to_string()}},
                {"key": "agentbox.guard_decision", "value": {"stringValue": s.guard_decision.as_deref().unwrap_or("allow")}},
                {"key": "agentbox.files", "value": {"stringValue": file_list.join(",")}},
            ],
            "status": {
                "code": if s.exit_status.unwrap_or(0) == 0 { 1 } else { 2 }
            }
        }));
    }

    let otlp = json!({
        "resourceSpans": [{
            "resource": {
                "attributes": [
                    {"key": "service.name", "value": {"stringValue": "agentbox"}},
                    {"key": "agentbox.project", "value": {"stringValue": project}}
                ]
            },
            "scopeSpans": [{
                "scope": {"name": "agentbox.audit", "version": env!("CARGO_PKG_VERSION")},
                "spans": spans
            }]
        }]
    });
    Ok(serde_json::to_string_pretty(&otlp)? + "\n")
}

fn export_markdown(db: &Database, steps: &[StepRecord], project: &str) -> Result<String> {
    use std::collections::BTreeSet;

    let agent_costs = db.cost_by("agent", Some(project)).unwrap_or_default();
    let total_cost: f64 = agent_costs.iter().map(|c| c.cost_usd).sum();
    let total_tokens: i64 = agent_costs
        .iter()
        .map(|c| c.input_tokens + c.output_tokens + c.cache_tokens)
        .sum();
    let guard_alerts = steps
        .iter()
        .filter(|s| matches!(s.guard_decision.as_deref(), Some("ask") | Some("deny")))
        .count();

    let mut unique_files = BTreeSet::new();
    for s in steps {
        for f in db.get_step_files(s.id).unwrap_or_default() {
            unique_files.insert(f.path);
        }
    }

    let mut md = String::new();
    md.push_str("### AgentBox Audit & Cost Summary\n\n");
    md.push_str(&format!(
        "- **Recorded Steps:** {}\n- **Files Touched:** {}\n- **Guard Alerts (`ask`/`deny`):** {}\n- **Total Tokens:** {}\n- **Est. API-Equiv. Cost:** ${:.2}\n\n",
        steps.len(),
        unique_files.len(),
        guard_alerts,
        crate::report::format_tokens(total_tokens),
        total_cost
    ));

    if !agent_costs.is_empty() {
        md.push_str("| Agent | Sessions | Steps | Tokens | API Equiv. ($) |\n");
        md.push_str("|---|---:|---:|---:|---:|\n");
        for c in &agent_costs {
            let t = c.input_tokens + c.output_tokens + c.cache_tokens;
            md.push_str(&format!(
                "| `{}` | {} | {} | {} | ${:.2} |\n",
                c.key,
                c.sessions,
                c.steps,
                crate::report::format_tokens(t),
                c.cost_usd
            ));
        }
        md.push('\n');
    }

    Ok(md)
}

fn hex_hash(input: &str, hex_len: usize) -> String {
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    let digest = h.finalize();
    let mut s = String::with_capacity(32);
    for b in digest.iter().take(hex_len / 2) {
        use std::fmt::Write;
        let _ = write!(&mut s, "{:02x}", b);
    }
    s
}
