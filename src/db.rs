use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub agent: String,
    pub project: String,
    pub project_hash: String,
    pub branch: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_tokens: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub id: i64,
    pub session_id: String,
    pub seq: i64,
    pub agent: String,
    pub tool: String,
    pub input_summary: String,
    pub input_json: String,
    pub pre_tree: Option<String>,
    pub post_tree: Option<String>,
    pub files_changed: i64,
    pub guard_decision: Option<String>,
    pub guard_rule: Option<String>,
    pub exit_status: Option<i64>,
    pub duration_ms: Option<i64>,
    pub started_at: i64,
    pub tokens: i64,
    pub cost_usd: f64,
    pub is_undo: bool,
    pub concurrent_overlap: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepFileRecord {
    pub step_id: i64,
    pub path: String,
    pub change: String, // A, M, D, R
}

#[derive(Debug, Clone)]
pub struct NewStepInput {
    pub session_id: String,
    pub agent: String,
    pub tool: String,
    pub input_summary: String,
    pub input_json: String,
    pub pre_tree: Option<String>,
    pub post_tree: Option<String>,
    pub files_changed: i64,
    pub guard_decision: Option<String>,
    pub guard_rule: Option<String>,
    pub exit_status: Option<i64>,
    pub duration_ms: Option<i64>,
    pub started_at: i64,
    pub tokens: i64,
    pub cost_usd: f64,
    pub is_undo: bool,
    pub concurrent_overlap: bool,
    pub tool_use_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LogFilter {
    pub project: Option<String>,
    pub all_sessions: bool,
    pub agent: Option<String>,
    pub since_ts: Option<i64>,
    pub file_path: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone)]
pub struct CostSummaryRow {
    pub key: String,
    pub sessions: i64,
    pub steps: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_tokens: i64,
    pub cost_usd: f64,
}

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(db_path)?;
        conn.busy_timeout(Duration::from_millis(5000))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS projects (
                hash TEXT PRIMARY KEY,
                root_path TEXT NOT NULL,
                baseline_tree TEXT,
                last_tree TEXT,
                last_step_id INTEGER,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                agent TEXT NOT NULL,
                project TEXT NOT NULL,
                project_hash TEXT NOT NULL,
                branch TEXT,
                started_at INTEGER NOT NULL,
                ended_at INTEGER,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_tokens INTEGER NOT NULL DEFAULT 0,
                cost_usd REAL NOT NULL DEFAULT 0.0
            );

            CREATE TABLE IF NOT EXISTS steps (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                agent TEXT NOT NULL,
                tool TEXT NOT NULL,
                input_summary TEXT NOT NULL,
                input_json TEXT NOT NULL,
                pre_tree TEXT,
                post_tree TEXT,
                files_changed INTEGER NOT NULL DEFAULT 0,
                guard_decision TEXT,
                guard_rule TEXT,
                exit_status INTEGER,
                duration_ms INTEGER,
                started_at INTEGER NOT NULL,
                tokens INTEGER NOT NULL DEFAULT 0,
                cost_usd REAL NOT NULL DEFAULT 0.0,
                is_undo INTEGER NOT NULL DEFAULT 0,
                concurrent_overlap INTEGER NOT NULL DEFAULT 0,
                tool_use_id TEXT
            );

            CREATE TABLE IF NOT EXISTS step_files (
                step_id INTEGER NOT NULL,
                path TEXT NOT NULL,
                change TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS inflight_tools (
                tool_use_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                project_hash TEXT NOT NULL,
                agent TEXT NOT NULL,
                tool TEXT NOT NULL,
                input_summary TEXT NOT NULL,
                input_json TEXT NOT NULL,
                pre_tree TEXT,
                guard_decision TEXT,
                guard_rule TEXT,
                started_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_sessions_project ON sessions(project, started_at DESC);
            CREATE INDEX IF NOT EXISTS idx_steps_session ON steps(session_id, seq);
            CREATE INDEX IF NOT EXISTS idx_steps_started ON steps(started_at DESC);
            CREATE INDEX IF NOT EXISTS idx_step_files_step ON step_files(step_id);
            CREATE INDEX IF NOT EXISTS idx_step_files_path ON step_files(path);
            CREATE INDEX IF NOT EXISTS idx_inflight_project ON inflight_tools(project_hash);
            "#,
        )?;
        Ok(())
    }

    pub fn upsert_project_baseline(
        &self,
        hash: &str,
        root_path: &str,
        baseline_tree: Option<&str>,
        last_tree: Option<&str>,
        now_ts: i64,
    ) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO projects(hash, root_path, baseline_tree, last_tree, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(hash) DO UPDATE SET
                root_path = excluded.root_path,
                baseline_tree = COALESCE(projects.baseline_tree, excluded.baseline_tree),
                last_tree = COALESCE(excluded.last_tree, projects.last_tree),
                updated_at = excluded.updated_at
            "#,
            params![hash, root_path, baseline_tree, last_tree, now_ts],
        )?;
        Ok(())
    }

    pub fn update_project_last_tree(
        &self,
        hash: &str,
        last_tree: &str,
        last_step_id: Option<i64>,
        now_ts: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE projects SET last_tree = ?2, last_step_id = COALESCE(?3, last_step_id), updated_at = ?4 WHERE hash = ?1",
            params![hash, last_tree, last_step_id, now_ts],
        )?;
        Ok(())
    }

    pub fn get_project_last_tree(&self, hash: &str) -> Result<Option<String>> {
        let res = self
            .conn
            .query_row(
                "SELECT last_tree FROM projects WHERE hash = ?1",
                params![hash],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(res)
    }

    pub fn ensure_session(
        &self,
        id: &str,
        agent: &str,
        project: &str,
        project_hash: &str,
        branch: Option<&str>,
        started_at: i64,
    ) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO sessions(id, agent, project, project_hash, branch, started_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(id) DO UPDATE SET
                branch = COALESCE(excluded.branch, sessions.branch)
            "#,
            params![id, agent, project, project_hash, branch, started_at],
        )?;
        Ok(())
    }

    pub fn finish_session(
        &self,
        id: &str,
        ended_at: i64,
        input_tokens: i64,
        output_tokens: i64,
        cache_tokens: i64,
        cost_usd: f64,
    ) -> Result<()> {
        self.conn.execute(
            r#"
            UPDATE sessions
            SET ended_at = ?2,
                input_tokens = CASE WHEN ?3 > 0 THEN ?3 ELSE input_tokens END,
                output_tokens = CASE WHEN ?4 > 0 THEN ?4 ELSE output_tokens END,
                cache_tokens = CASE WHEN ?5 > 0 THEN ?5 ELSE cache_tokens END,
                cost_usd = CASE WHEN ?6 > 0 THEN ?6 ELSE cost_usd END
            WHERE id = ?1
            "#,
            params![id, ended_at, input_tokens, output_tokens, cache_tokens, cost_usd],
        )?;
        Ok(())
    }

    pub fn update_step_costs_by_tool_use_id(
        &mut self,
        session_id: &str,
        tool_costs: &std::collections::HashMap<String, (i64, f64)>,
    ) -> Result<usize> {
        if tool_costs.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.transaction()?;
        let mut updated = 0usize;
        {
            let mut stmt = tx.prepare(
                "UPDATE steps SET tokens = ?1, cost_usd = ?2 WHERE session_id = ?3 AND tool_use_id = ?4",
            )?;
            for (tid, (toks, cost)) in tool_costs {
                updated += stmt.execute(params![toks, cost, session_id, tid])?;
            }
        }
        tx.commit()?;
        Ok(updated)
    }

    pub fn record_inflight_start(
        &self,
        tool_use_id: &str,
        session_id: &str,
        project_hash: &str,
        agent: &str,
        tool: &str,
        input_summary: &str,
        input_json: &str,
        pre_tree: Option<&str>,
        guard_decision: Option<&str>,
        guard_rule: Option<&str>,
        started_at: i64,
    ) -> Result<bool> {
        // Detect if any other inflight tool exists for this project
        let active_others: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM inflight_tools WHERE project_hash = ?1 AND tool_use_id != ?2",
            params![project_hash, tool_use_id],
            |row| row.get(0),
        )?;

        self.conn.execute(
            r#"
            INSERT OR REPLACE INTO inflight_tools(
                tool_use_id, session_id, project_hash, agent, tool,
                input_summary, input_json, pre_tree, guard_decision, guard_rule, started_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            "#,
            params![
                tool_use_id,
                session_id,
                project_hash,
                agent,
                tool,
                input_summary,
                input_json,
                pre_tree,
                guard_decision,
                guard_rule,
                started_at
            ],
        )?;
        Ok(active_others > 0)
    }

    pub fn take_inflight(
        &self,
        tool_use_id: Option<&str>,
        session_id: &str,
        tool: &str,
    ) -> Result<(Option<InflightTool>, bool)> {
        let row = if let Some(id) = tool_use_id {
            self.conn
                .query_row(
                    r#"
                    SELECT tool_use_id, session_id, project_hash, agent, tool,
                           input_summary, input_json, pre_tree, guard_decision, guard_rule, started_at
                    FROM inflight_tools
                    WHERE tool_use_id = ?1
                    "#,
                    params![id],
                    InflightTool::from_row,
                )
                .optional()?
        } else {
            self.conn
                .query_row(
                    r#"
                    SELECT tool_use_id, session_id, project_hash, agent, tool,
                           input_summary, input_json, pre_tree, guard_decision, guard_rule, started_at
                    FROM inflight_tools
                    WHERE session_id = ?1 AND tool = ?2
                    ORDER BY started_at DESC
                    LIMIT 1
                    "#,
                    params![session_id, tool],
                    InflightTool::from_row,
                )
                .optional()?
        };

        let mut concurrent_overlap = false;
        if let Some(ref found) = row {
            let others: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM inflight_tools WHERE project_hash = ?1 AND tool_use_id != ?2",
                params![found.project_hash, found.tool_use_id],
                |r| r.get(0),
            )?;
            concurrent_overlap = others > 0;
            self.conn.execute(
                "DELETE FROM inflight_tools WHERE tool_use_id = ?1",
                params![found.tool_use_id],
            )?;
        }

        Ok((row, concurrent_overlap))
    }

    pub fn insert_step(&mut self, input: &NewStepInput, files: &[(String, String)]) -> Result<i64> {
        let tx = self.conn.transaction()?;
        let next_seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM steps WHERE session_id = ?1",
            params![input.session_id],
            |row| row.get(0),
        )?;

        tx.execute(
            r#"
            INSERT INTO steps(
                session_id, seq, agent, tool, input_summary, input_json,
                pre_tree, post_tree, files_changed, guard_decision, guard_rule,
                exit_status, duration_ms, started_at, tokens, cost_usd,
                is_undo, concurrent_overlap, tool_use_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
            "#,
            params![
                input.session_id,
                next_seq,
                input.agent,
                input.tool,
                input.input_summary,
                input.input_json,
                input.pre_tree,
                input.post_tree,
                input.files_changed,
                input.guard_decision,
                input.guard_rule,
                input.exit_status,
                input.duration_ms,
                input.started_at,
                input.tokens,
                input.cost_usd,
                if input.is_undo { 1 } else { 0 },
                if input.concurrent_overlap { 1 } else { 0 },
                input.tool_use_id,
            ],
        )?;

        let step_id = tx.last_insert_rowid();
        {
            let mut stmt = tx.prepare(
                "INSERT INTO step_files(step_id, path, change) VALUES (?1, ?2, ?3)",
            )?;
            for (path, change) in files {
                stmt.execute(params![step_id, path, change])?;
            }
        }
        tx.commit()?;
        Ok(step_id)
    }

    pub fn latest_session_for_project(&self, project: &str) -> Result<Option<SessionRecord>> {
        let res = self
            .conn
            .query_row(
                r#"
                SELECT id, agent, project, project_hash, branch, started_at, ended_at,
                       input_tokens, output_tokens, cache_tokens, cost_usd
                FROM sessions
                WHERE project = ?1
                ORDER BY started_at DESC
                LIMIT 1
                "#,
                params![project],
                SessionRecord::from_row,
            )
            .optional()?;
        Ok(res)
    }

    #[allow(dead_code)]
    pub fn get_session(&self, session_id: &str) -> Result<Option<SessionRecord>> {
        let res = self
            .conn
            .query_row(
                r#"
                SELECT id, agent, project, project_hash, branch, started_at, ended_at,
                       input_tokens, output_tokens, cache_tokens, cost_usd
                FROM sessions
                WHERE id = ?1
                "#,
                params![session_id],
                SessionRecord::from_row,
            )
            .optional()?;
        Ok(res)
    }

    pub fn query_steps(&self, filter: &LogFilter) -> Result<Vec<StepRecord>> {
        let mut sql = String::from(
            r#"
            SELECT s.id, s.session_id, s.seq, s.agent, s.tool, s.input_summary, s.input_json,
                   s.pre_tree, s.post_tree, s.files_changed, s.guard_decision, s.guard_rule,
                   s.exit_status, s.duration_ms, s.started_at, s.tokens, s.cost_usd,
                   s.is_undo, s.concurrent_overlap
            FROM steps s
            JOIN sessions sess ON sess.id = s.session_id
            WHERE 1=1
            "#,
        );
        let mut owned_params: Vec<rusqlite::types::Value> = Vec::new();

        if let Some(ref proj) = filter.project {
            if filter.all_sessions {
                sql.push_str(" AND sess.project = ?");
                owned_params.push(proj.clone().into());
            } else {
                sql.push_str(
                    " AND sess.id = (SELECT id FROM sessions WHERE project = ? ORDER BY started_at DESC LIMIT 1)",
                );
                owned_params.push(proj.clone().into());
            }
        }
        if let Some(ref agent) = filter.agent {
            sql.push_str(" AND s.agent = ?");
            owned_params.push(agent.clone().into());
        }
        if let Some(since_ts) = filter.since_ts {
            sql.push_str(" AND s.started_at >= ?");
            owned_params.push(since_ts.into());
        }
        if let Some(ref file_path) = filter.file_path {
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM step_files sf WHERE sf.step_id = s.id AND sf.path LIKE ?)",
            );
            owned_params.push(format!("%{}%", file_path).into());
        }

        sql.push_str(" ORDER BY s.id DESC LIMIT ?");
        owned_params.push((filter.limit as i64).into());

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(owned_params.iter()), StepRecord::from_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        out.reverse(); // chronological order for display
        Ok(out)
    }

    pub fn get_step(&self, step_id: i64) -> Result<Option<StepRecord>> {
        let res = self
            .conn
            .query_row(
                r#"
                SELECT id, session_id, seq, agent, tool, input_summary, input_json,
                       pre_tree, post_tree, files_changed, guard_decision, guard_rule,
                       exit_status, duration_ms, started_at, tokens, cost_usd,
                       is_undo, concurrent_overlap
                FROM steps
                WHERE id = ?1
                "#,
                params![step_id],
                StepRecord::from_row,
            )
            .optional()?;
        Ok(res)
    }

    pub fn get_latest_mutating_step_for_project(&self, project: &str) -> Result<Option<StepRecord>> {
        let res = self
            .conn
            .query_row(
                r#"
                SELECT s.id, s.session_id, s.seq, s.agent, s.tool, s.input_summary, s.input_json,
                       s.pre_tree, s.post_tree, s.files_changed, s.guard_decision, s.guard_rule,
                       s.exit_status, s.duration_ms, s.started_at, s.tokens, s.cost_usd,
                       s.is_undo, s.concurrent_overlap
                FROM steps s
                JOIN sessions sess ON sess.id = s.session_id
                WHERE sess.project = ?1 AND s.pre_tree IS NOT NULL AND s.post_tree IS NOT NULL
                  AND (s.files_changed > 0 OR s.is_undo = 1)
                ORDER BY s.id DESC
                LIMIT 1
                "#,
                params![project],
                StepRecord::from_row,
            )
            .optional()?;
        Ok(res)
    }

    pub fn get_step_files(&self, step_id: i64) -> Result<Vec<StepFileRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT step_id, path, change FROM step_files WHERE step_id = ?1 ORDER BY path")?;
        let rows = stmt.query_map(params![step_id], |row| {
            Ok(StepFileRecord {
                step_id: row.get(0)?,
                path: row.get(1)?,
                change: row.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn cost_by(&self, by: &str, project: Option<&str>) -> Result<Vec<CostSummaryRow>> {
        let group_expr = match by {
            "branch" => "COALESCE(branch, '(detached/unknown)')",
            "agent" => "agent",
            _ => "id",
        };
        let mut sql = format!(
            r#"
            SELECT {group_expr} AS grp,
                   COUNT(DISTINCT id) AS sess_cnt,
                   COALESCE((SELECT COUNT(*) FROM steps st WHERE st.session_id IN (
                       SELECT id FROM sessions s2 WHERE {group_expr_s2} = {group_expr}
                   )), 0) AS step_cnt,
                   COALESCE(SUM(input_tokens), 0),
                   COALESCE(SUM(output_tokens), 0),
                   COALESCE(SUM(cache_tokens), 0),
                   COALESCE(SUM(cost_usd), 0.0)
            FROM sessions
            WHERE 1=1
            "#,
            group_expr = group_expr,
            group_expr_s2 = group_expr.replace("branch", "s2.branch").replace("agent", "s2.agent").replace("id", "s2.id")
        );
        let mut params_vec: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(proj) = project {
            sql.push_str(" AND project = ?");
            params_vec.push(proj.to_string().into());
        }
        sql.push_str(&format!(" GROUP BY {group_expr} ORDER BY SUM(cost_usd) DESC, MAX(started_at) DESC LIMIT 50"));

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params_vec.iter()), |row| {
            Ok(CostSummaryRow {
                key: row.get(0)?,
                sessions: row.get(1)?,
                steps: row.get(2)?,
                input_tokens: row.get(3)?,
                output_tokens: row.get(4)?,
                cache_tokens: row.get(5)?,
                cost_usd: row.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn delete_sessions_older_than(&mut self, cutoff_ts: i64) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, project_hash FROM sessions WHERE started_at < ?1")?;
        let targets: Vec<(String, String)> = stmt
            .query_map(params![cutoff_ts], |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);

        if targets.is_empty() {
            return Ok(Vec::new());
        }

        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM step_files WHERE step_id IN (SELECT id FROM steps WHERE session_id IN (SELECT id FROM sessions WHERE started_at < ?1))",
            params![cutoff_ts],
        )?;
        tx.execute(
            "DELETE FROM steps WHERE session_id IN (SELECT id FROM sessions WHERE started_at < ?1)",
            params![cutoff_ts],
        )?;
        tx.execute("DELETE FROM sessions WHERE started_at < ?1", params![cutoff_ts])?;
        tx.commit()?;
        Ok(targets)
    }

    pub fn delete_oldest_session(&mut self) -> Result<Option<(String, String)>> {
        let target: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT id, project_hash FROM sessions ORDER BY started_at ASC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        let Some((sid, phash)) = target else {
            return Ok(None);
        };

        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM step_files WHERE step_id IN (SELECT id FROM steps WHERE session_id = ?1)",
            params![&sid],
        )?;
        tx.execute("DELETE FROM steps WHERE session_id = ?1", params![&sid])?;
        tx.execute("DELETE FROM sessions WHERE id = ?1", params![&sid])?;
        tx.commit()?;
        Ok(Some((sid, phash)))
    }

    pub fn stats_counts(&self) -> Result<(i64, i64, i64)> {
        let projects: i64 = self.conn.query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))?;
        let sessions: i64 = self.conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
        let steps: i64 = self.conn.query_row("SELECT COUNT(*) FROM steps", [], |r| r.get(0))?;
        Ok((projects, sessions, steps))
    }
}

#[derive(Debug, Clone)]
pub struct InflightTool {
    pub tool_use_id: String,
    #[allow(dead_code)]
    pub session_id: String,
    pub project_hash: String,
    #[allow(dead_code)]
    pub agent: String,
    #[allow(dead_code)]
    pub tool: String,
    pub input_summary: String,
    pub input_json: String,
    pub pre_tree: Option<String>,
    pub guard_decision: Option<String>,
    pub guard_rule: Option<String>,
    pub started_at: i64,
}

impl InflightTool {
    fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            tool_use_id: row.get(0)?,
            session_id: row.get(1)?,
            project_hash: row.get(2)?,
            agent: row.get(3)?,
            tool: row.get(4)?,
            input_summary: row.get(5)?,
            input_json: row.get(6)?,
            pre_tree: row.get(7)?,
            guard_decision: row.get(8)?,
            guard_rule: row.get(9)?,
            started_at: row.get(10)?,
        })
    }
}

impl SessionRecord {
    fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            agent: row.get(1)?,
            project: row.get(2)?,
            project_hash: row.get(3)?,
            branch: row.get(4)?,
            started_at: row.get(5)?,
            ended_at: row.get(6)?,
            input_tokens: row.get(7)?,
            output_tokens: row.get(8)?,
            cache_tokens: row.get(9)?,
            cost_usd: row.get(10)?,
        })
    }
}

impl StepRecord {
    fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            session_id: row.get(1)?,
            seq: row.get(2)?,
            agent: row.get(3)?,
            tool: row.get(4)?,
            input_summary: row.get(5)?,
            input_json: row.get(6)?,
            pre_tree: row.get(7)?,
            post_tree: row.get(8)?,
            files_changed: row.get(9)?,
            guard_decision: row.get(10)?,
            guard_rule: row.get(11)?,
            exit_status: row.get(12)?,
            duration_ms: row.get(13)?,
            started_at: row.get(14)?,
            tokens: row.get(15)?,
            cost_usd: row.get(16)?,
            is_undo: row.get::<_, i64>(17)? != 0,
            concurrent_overlap: row.get::<_, i64>(18)? != 0,
        })
    }
}
