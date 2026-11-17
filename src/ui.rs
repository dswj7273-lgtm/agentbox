use crate::commands::execute_undo;
use crate::db::{Database, LogFilter};
use crate::paths::{project_hash, resolve_project_root, AgentBoxPaths};
use crate::snapshot::diff_patch;
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;

pub fn cmd_ui(port: u16, no_open: bool) -> Result<()> {
    let addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&addr)?;
    let url = format!("http://{}", addr);
    println!("✔ AgentBox UI running at {} (Ctrl+C to stop)", url);

    if !no_open {
        open_browser(&url);
    }

    for stream in listener.incoming().flatten() {
        let _ = handle_connection(stream);
    }
    Ok(())
}

pub fn handle_http_request(
    method: &str,
    path_and_query: &str,
    body: &str,
    cwd: &Path,
) -> (u16, &'static str, String) {
    let (path, query) = path_and_query.split_once('?').unwrap_or((path_and_query, ""));
    match (method, path) {
        ("GET", "/") => (200, "text/html; charset=utf-8", UI_HTML.to_string()),
        ("GET", "/api/steps") => match api_steps(cwd, query) {
            Ok(json_str) => (200, "application/json", json_str),
            Err(e) => (500, "application/json", json!({"error": e.to_string()}).to_string()),
        },
        ("GET", "/api/diff") => match api_diff(cwd, query) {
            Ok(json_str) => (200, "application/json", json_str),
            Err(e) => (400, "application/json", json!({"error": e.to_string()}).to_string()),
        },
        ("POST", "/api/undo") => match api_undo(body) {
            Ok(json_str) => (200, "application/json", json_str),
            Err(e) => (400, "application/json", json!({"error": e.to_string()}).to_string()),
        },
        _ => (404, "application/json", json!({"error": "Not Found"}).to_string()),
    }
}

fn handle_connection(mut stream: TcpStream) -> Result<()> {
    let mut buf = [0u8; 16384];
    let n = stream.read(&mut buf)?;
    if n == 0 {
        return Ok(());
    }
    let raw = String::from_utf8_lossy(&buf[..n]);
    let mut lines = raw.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let path_and_query = parts.next().unwrap_or("/");

    let body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
    let cwd = std::env::current_dir()?;
    let (status, content_type, resp_body) = handle_http_request(method, path_and_query, body, &cwd);

    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Internal Server Error",
    };

    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        status_text,
        content_type,
        resp_body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(resp_body.as_bytes())?;
    Ok(())
}

fn api_steps(cwd: &Path, query: &str) -> Result<String> {
    let paths = AgentBoxPaths::resolve()?;
    let project_root = resolve_project_root(cwd);
    let proj_str = project_root.to_string_lossy().to_string();

    if !paths.db_path().exists() {
        return Ok(json!({"project": proj_str, "steps": [], "agents": []}).to_string());
    }

    let db = Database::open(&paths.db_path())?;
    let all_sessions = query.contains("all=1");
    let agent = query_param(query, "agent").filter(|s| !s.is_empty());

    let steps = db.query_steps(&LogFilter {
        project: Some(proj_str.clone()),
        all_sessions,
        agent,
        since_ts: None,
        file_path: None,
        limit: 200,
    })?;
    let costs = db.cost_by("agent", Some(&proj_str)).unwrap_or_default();
    let agents: Vec<_> = costs
        .into_iter()
        .map(|c| {
            json!({
                "agent": c.key,
                "sessions": c.sessions,
                "steps": c.steps,
                "tokens": c.input_tokens + c.output_tokens + c.cache_tokens,
                "cost_usd": c.cost_usd
            })
        })
        .collect();

    Ok(json!({
        "project": proj_str,
        "steps": steps,
        "agents": agents
    })
    .to_string())
}

fn api_diff(cwd: &Path, query: &str) -> Result<String> {
    let step_id: i64 = query_param(query, "step")
        .ok_or_else(|| anyhow::anyhow!("Missing ?step=ID"))?
        .parse()?;
    let paths = AgentBoxPaths::resolve()?;
    let db = Database::open(&paths.db_path())?;
    let step = db
        .get_step(step_id)?
        .ok_or_else(|| anyhow::anyhow!("Step #{} not found", step_id))?;
    let files = db.get_step_files(step_id)?;

    let project_root = resolve_project_root(cwd);
    let proj_hash = project_hash(&project_root);
    let shadow_repo = paths.shadow_repo_dir(&proj_hash);

    let patch = match (&step.pre_tree, &step.post_tree) {
        (Some(a), Some(b)) if a != b => diff_patch(&shadow_repo, a, b, false).unwrap_or_default(),
        _ => String::new(),
    };

    Ok(json!({
        "step": step,
        "files": files,
        "patch": patch
    })
    .to_string())
}

#[derive(Deserialize)]
struct UndoRequest {
    step_id: i64,
    #[serde(default)]
    only: Vec<String>,
    #[serde(default)]
    force: bool,
}

fn api_undo(body: &str) -> Result<String> {
    let req: UndoRequest = serde_json::from_str(body)?;
    let outcome = execute_undo(None, Some(req.step_id), req.only, req.force, true, true)?;
    Ok(serde_json::to_string(&outcome)?)
}

fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

const UI_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<title>AgentBox · Flight Recorder</title>
<style>
  :root { --bg:#0b1120; --panel:#111827; --border:#1f2937; --text:#e5e7eb; --muted:#9ca3af; --accent:#38bdf8; --danger:#f87171; --ok:#34d399; }
  * { box-sizing: border-box; }
  body { margin:0; font-family: ui-sans-serif, system-ui, -apple-system, sans-serif; background:var(--bg); color:var(--text); display:flex; flex-direction:column; height:100vh; }
  header { padding:14px 20px; border-bottom:1px solid var(--border); display:flex; justify-content:space-between; align-items:center; background:var(--panel); }
  .brand { font-weight:700; font-size:16px; display:flex; gap:10px; align-items:center; }
  .badge { font-size:12px; padding:2px 8px; border-radius:999px; background:#1e293b; color:var(--accent); }
  .controls { display:flex; gap:8px; align-items:center; }
  button, select { background:#1e293b; color:var(--text); border:1px solid #334155; border-radius:6px; padding:6px 12px; cursor:pointer; font-size:13px; }
  button.primary { background:#0284c7; border-color:#0284c7; color:white; font-weight:600; }
  button.danger { background:#991b1b; border-color:#dc2626; color:white; font-weight:600; }
  main { display:grid; grid-template-columns: 440px 1fr; flex:1; overflow:hidden; }
  .list { border-right:1px solid var(--border); overflow-y:auto; padding:12px; display:flex; flex-direction:column; gap:8px; }
  .step { padding:10px 12px; border:1px solid var(--border); border-radius:8px; background:var(--panel); cursor:pointer; }
  .step.active { border-color:var(--accent); background:#0f172a; }
  .step-top { display:flex; justify-content:space-between; font-size:12px; color:var(--muted); margin-bottom:4px; }
  .step-sum { font-family: ui-monospace, monospace; font-size:13px; white-space:nowrap; overflow:hidden; text-overflow:ellipsis; }
  .detail { overflow-y:auto; padding:18px 22px; }
  pre.diff { background:#050811; border:1px solid var(--border); border-radius:8px; padding:14px; font-size:12px; overflow-x:auto; line-height:1.45; }
  .d-add { color: #4ade80; }
  .d-del { color: #f87171; }
  .d-hdr { color: #38bdf8; }
  .summary-bar { display:flex; gap:12px; flex-wrap:wrap; margin-bottom:14px; }
  .pill { background:#1e293b; padding:6px 10px; border-radius:6px; font-size:12px; }
</style>
</head>
<body>
<header>
  <div class="brand">AgentBox <span class="badge" id="proj">Loading...</span></div>
  <div class="controls">
    <select id="agentFilter"><option value="">All Agents</option><option value="claude">claude</option><option value="codex">codex</option><option value="gemini">gemini</option><option value="abox">abox</option></select>
    <label style="font-size:13px"><input type="checkbox" id="allSessions" checked /> All sessions</label>
    <button onclick="loadSteps()">Refresh</button>
  </div>
</header>
<main>
  <div class="list" id="stepList"></div>
  <div class="detail" id="detailPane"><p style="color:var(--muted)">Select a step to inspect its diff and undo.</p></div>
</main>
<script>
let selectedId = null;
async function loadSteps() {
  const all = document.getElementById('allSessions').checked ? '1' : '0';
  const agent = document.getElementById('agentFilter').value;
  const res = await fetch(`/api/steps?all=${all}&agent=${encodeURIComponent(agent)}`);
  const data = await res.json();
  document.getElementById('proj').textContent = data.project || '-';
  const list = document.getElementById('stepList');
  list.innerHTML = '';
  const steps = (data.steps || []).slice().reverse();
  if (!steps.length) { list.innerHTML = '<div style="color:#9ca3af;padding:12px">No steps recorded yet.</div>'; return; }
  for (const s of steps) {
    const el = document.createElement('div');
    el.className = 'step' + (s.id === selectedId ? ' active' : '');
    const t = new Date(s.started_at * 1000).toLocaleTimeString();
    el.innerHTML = `<div class="step-top"><span>#${s.id} · ${s.agent} · ${s.tool}</span><span>${s.files_changed} files · ${t}</span></div>
      <div class="step-sum">${esc(s.input_summary)}</div>`;
    el.onclick = () => selectStep(s.id);
    list.appendChild(el);
  }
}
async function selectStep(id) {
  selectedId = id;
  loadSteps();
  const res = await fetch(`/api/diff?step=${id}`);
  const d = await res.json();
  const s = d.step;
  const files = (d.files || []).map(f => `<li><code>${esc(f.change)} ${esc(f.path)}</code></li>`).join('');
  const undoBtn = s.pre_tree ? `<button class="danger" onclick="undoStep(${s.id}, false)">Undo to before #${s.id}</button>` : '';
  document.getElementById('detailPane').innerHTML = `
    <div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:12px">
      <h2 style="margin:0;font-size:18px">Step #${s.id}: ${esc(s.tool)} (${esc(s.agent)})</h2>
      ${undoBtn}
    </div>
    <div class="summary-bar">
      <div class="pill">Summary: <code>${esc(s.input_summary)}</code></div>
      <div class="pill">Guard: ${esc(s.guard_decision || 'allow')}</div>
      <div class="pill">Exit: ${s.exit_status ?? '-'}</div>
    </div>
    ${files ? `<ul>${files}</ul>` : '<p style="color:#9ca3af">No filesystem changes.</p>'}
    <pre class="diff">${colorDiff(d.patch || '(no diff)')}</pre>
  `;
}
async function undoStep(id, force) {
  const res = await fetch('/api/undo', { method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify({step_id:id, force}) });
  const out = await res.json();
  if (out.status === 'Conflict') {
    if (confirm('Manual edits detected outside AgentBox:\\n' + out.conflict_files.map(x=>x[1]+' '+x[0]).join('\\n') + '\\n\\nForce overwrite?')) {
      return undoStep(id, true);
    }
    return;
  }
  alert('Undo status: ' + out.status + (out.restored_files ? ` (${out.restored_files} files restored, undo step #${out.undo_step_id})` : ''));
  loadSteps();
}
function esc(s){ return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;'); }
function colorDiff(p){
  return p.split('\n').map(l => {
    const e = esc(l);
    if (l.startsWith('+') && !l.startsWith('+++')) return `<span class="d-add">${e}</span>`;
    if (l.startsWith('-') && !l.startsWith('---')) return `<span class="d-del">${e}</span>`;
    if (l.startsWith('@@') || l.startsWith('diff ')) return `<span class="d-hdr">${e}</span>`;
    return e;
  }).join('\n');
}
document.getElementById('agentFilter').onchange = loadSteps;
document.getElementById('allSessions').onchange = loadSteps;
loadSteps();
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_http_routes_return_expected_responses() {
        let cwd = std::env::temp_dir();
        let (status, ct, body) = handle_http_request("GET", "/", "", &cwd);
        assert_eq!(status, 200);
        assert!(ct.contains("text/html"));
        assert!(body.contains("AgentBox"));

        let (status_404, _, _) = handle_http_request("GET", "/nonexistent", "", &cwd);
        assert_eq!(status_404, 404);
    }
}
