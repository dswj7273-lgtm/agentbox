mod adapter;
mod commands;
mod db;
mod export;
mod guard;
mod hook;
mod paths;
mod pricing;
mod report;
mod snapshot;
mod ui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "abox",
    version,
    about = "AgentBox: local flight recorder, guard, and step-by-step undo for AI coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Detect installed AI coding agents, install hooks, and initialize shadow repo
    Init {
        /// Install hooks into current project only (.claude/settings.json)
        #[arg(long)]
        project: bool,
        /// Install hooks globally (~/.claude/settings.json)
        #[arg(long)]
        global: bool,
    },
    /// Remove AgentBox hooks cleanly
    Uninstall {
        #[arg(long)]
        project: bool,
        #[arg(long)]
        global: bool,
        /// Also delete ~/.agentbox database and shadow repositories
        #[arg(long)]
        purge: bool,
    },
    /// Internal hook entrypoint invoked by AI coding agents (reads JSON from stdin)
    Hook {
        /// Agent name (e.g., claude)
        agent: String,
    },
    /// Show step timeline for the current project
    Log {
        /// Show all sessions in this project (default: latest session)
        #[arg(long)]
        all: bool,
        /// Filter by agent name (e.g., claude)
        #[arg(long)]
        agent: Option<String>,
        /// Filter by relative duration (e.g., 2h, 7d, 30m)
        #[arg(long)]
        since: Option<String>,
        /// Filter steps that touched a specific file path
        #[arg(long)]
        file: Option<String>,
        /// Max number of steps to display
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Show detailed metadata and changed files for a step
    Show {
        /// Step ID (# from `abox log`)
        step: i64,
    },
    /// Show diff for a step (`43`) or step range (`40..44`)
    Diff {
        /// Step ID or range A..B
        target: String,
        /// Output plain unified patch without header/color
        #[arg(long)]
        patch: bool,
    },
    /// Undo agent changes safely (creates a revertible undo snapshot first)
    Undo {
        /// Optional step ID (defaults to latest mutating step)
        step: Option<i64>,
        /// Restore workspace to state right before this step
        #[arg(long)]
        to: Option<i64>,
        /// Only restore specific file path(s)
        #[arg(long = "only")]
        only: Vec<String>,
        /// Overwrite external/manual modifications without aborting
        #[arg(long)]
        force: bool,
        /// Skip interactive confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Scan historical local agent logs (no hooks needed) and print audit summary
    Report {
        /// Lookback window in days
        #[arg(long, default_value_t = 30)]
        days: u64,
        /// Custom directory containing agent JSONL logs
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Output JSON instead of terminal card
        #[arg(long)]
        json: bool,
        /// Export standalone HTML report
        #[arg(long)]
        html: Option<PathBuf>,
    },
    /// Summarize token and USD cost by branch, session, or agent
    Cost {
        #[arg(long, default_value = "session", value_parser = ["branch", "session", "agent"])]
        by: String,
        /// Aggregate across all projects
        #[arg(long)]
        all: bool,
    },
    /// Export audit timeline in JSONL or OpenTelemetry (OTLP/JSON) format
    Export {
        #[arg(long, default_value = "jsonl", value_parser = ["jsonl", "otel"])]
        format: String,
        /// Export all sessions for this project
        #[arg(long)]
        all: bool,
        #[arg(long, default_value_t = 500)]
        limit: usize,
        /// Write output to file instead of stdout
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Launch local Web UI to browse timeline, inspect diffs, and undo steps
    Ui {
        #[arg(long, default_value_t = 4317)]
        port: u16,
        /// Do not auto-open browser window
        #[arg(long)]
        no_open: bool,
    },
    /// Prune old sessions and compact shadow git repositories
    Gc {
        #[arg(long, default_value_t = 14)]
        days: u64,
        #[arg(long, default_value_t = 2.0)]
        max_gb: f64,
    },
    /// Check installation, git, hooks, database health, and error logs
    Doctor,
    /// Run M0 shadow-git snapshot latency benchmark
    Bench {
        #[arg(long, default_value_t = 10000)]
        files: usize,
        #[arg(long, default_value_t = 20)]
        iters: usize,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Init { project, global } => commands::cmd_init(project, global),
        Commands::Uninstall {
            project,
            global,
            purge,
        } => commands::cmd_uninstall(project, global, purge),
        Commands::Hook { agent } => hook::run_hook(&agent),
        Commands::Log {
            all,
            agent,
            since,
            file,
            limit,
        } => commands::cmd_log(all, agent, since, file, limit),
        Commands::Show { step } => commands::cmd_show(step),
        Commands::Diff { target, patch } => commands::cmd_diff(&target, patch),
        Commands::Undo {
            step,
            to,
            only,
            force,
            yes,
        } => commands::cmd_undo(step, to, only, force, yes),
        Commands::Report {
            days,
            dir,
            json,
            html,
        } => report::generate_report(days, dir.as_deref(), json, html.as_deref()),
        Commands::Cost { by, all } => commands::cmd_cost(&by, all),
        Commands::Export {
            format,
            all,
            limit,
            out,
        } => export::cmd_export(&format, all, limit, out.as_deref()),
        Commands::Ui { port, no_open } => ui::cmd_ui(port, no_open),
        Commands::Gc { days, max_gb } => commands::cmd_gc(days, max_gb),
        Commands::Doctor => commands::cmd_doctor(),
        Commands::Bench { files, iters, repo } => commands::cmd_bench(files, iters, repo),
    }
}
