mod adapter;
mod commands;
mod completions;
mod db;
mod export;
mod guard;
mod hook;
mod paths;
mod pricing;
mod report;
mod snapshot;
mod ui;
mod watch;

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
        /// Only install hooks for a specific agent (claude, codex, gemini, opencode)
        #[arg(long)]
        agent: Option<String>,
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
        /// Output JSON array instead of table
        #[arg(long)]
        json: bool,
        /// Follow and stream newly recorded steps in real time
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// Show detailed metadata and changed files for a step
    Show {
        /// Step ID (# from `abox log`)
        step: i64,
        /// Output JSON instead of formatted text
        #[arg(long)]
        json: bool,
    },
    /// Show diff for a step (`43`), step range (`40..44`), or checkpoint (`cp:<name>`)
    Diff {
        /// Step ID, range A..B, or cp:<name>
        target: String,
        /// Output plain unified patch without header/color
        #[arg(long)]
        patch: bool,
        /// Show diffstat summary only
        #[arg(long)]
        stat: bool,
    },
    /// Undo agent changes safely (creates a revertible undo snapshot first)
    Undo {
        /// Optional step ID (defaults to latest mutating step)
        step: Option<i64>,
        /// Restore workspace to state right before this step
        #[arg(long)]
        to: Option<i64>,
        /// Restore workspace to a named checkpoint created with `abox checkpoint`
        #[arg(long)]
        to_tag: Option<String>,
        /// Only restore specific file path(s)
        #[arg(long = "only")]
        only: Vec<String>,
        /// Overwrite external/manual modifications without aborting
        #[arg(long)]
        force: bool,
        /// Skip interactive confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
        /// Preview files that would be restored/deleted without modifying disk
        #[arg(long)]
        dry_run: bool,
    },
    /// Save or list named workspace checkpoints
    Checkpoint {
        /// Checkpoint name (omit or pass --list to view saved checkpoints)
        name: Option<String>,
        /// List saved checkpoints for the current project
        #[arg(long)]
        list: bool,
    },
    /// Show AgentBox hook, snapshot, and workspace status for the current project
    Status,
    /// List all repositories tracked by AgentBox with step counts and shadow storage size
    Projects,
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
    /// Export audit timeline in JSONL, OpenTelemetry (OTLP/JSON), or Markdown PR comment format
    Export {
        #[arg(long, default_value = "jsonl", value_parser = ["jsonl", "otel", "markdown"])]
        format: String,
        /// Export all sessions for this project
        #[arg(long)]
        all: bool,
        /// Filter exported steps by agent name
        #[arg(long)]
        agent: Option<String>,
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
    /// Watch filesystem changes as a fallback for agents without hooks (e.g., Cursor)
    Watch {
        /// Agent label recorded in timeline
        #[arg(long, default_value = "cursor")]
        agent: String,
        /// Polling interval in milliseconds
        #[arg(long, default_value_t = 1500)]
        interval_ms: u64,
        /// Optional maximum poll cycles before exiting
        #[arg(long)]
        cycles: Option<usize>,
    },
    /// Prune old sessions and compact shadow git repositories
    Gc {
        #[arg(long, default_value_t = 14)]
        days: u64,
        #[arg(long, default_value_t = 2.0)]
        max_gb: f64,
    },
    /// Inspect, initialize, or dry-run Guard policy rules
    Policy {
        #[command(subcommand)]
        action: PolicyAction,
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
    /// Print shell completion script (bash, zsh, fish)
    Completions {
        #[arg(value_parser = ["bash", "zsh", "fish"])]
        shell: String,
    },
}

#[derive(Subcommand)]
enum PolicyAction {
    /// Generate .agentbox/policy.yaml and .aboxignore templates in current project
    Init {
        #[arg(long)]
        force: bool,
    },
    /// List active merged Guard rules
    List,
    /// Dry-run Guard evaluation against a tool and command/path
    Test {
        /// Tool name (e.g., Bash, Write, Edit)
        tool: String,
        /// Command string (for Bash) or file path (for Write/Edit)
        target: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Init {
            project,
            global,
            agent,
        } => commands::cmd_init(project, global, agent),
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
            json,
            follow,
        } => commands::cmd_log(all, agent, since, file, limit, json, follow),
        Commands::Show { step, json } => commands::cmd_show(step, json),
        Commands::Diff {
            target,
            patch,
            stat,
        } => commands::cmd_diff(&target, patch, stat),
        Commands::Undo {
            step,
            to,
            to_tag,
            only,
            force,
            yes,
            dry_run,
        } => commands::cmd_undo(step, to, to_tag, only, force, yes, dry_run),
        Commands::Checkpoint { name, list } => commands::cmd_checkpoint(name, list),
        Commands::Status => commands::cmd_status(),
        Commands::Projects => commands::cmd_projects(),
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
            agent,
            limit,
            out,
        } => export::cmd_export(&format, all, agent, limit, out.as_deref()),
        Commands::Ui { port, no_open } => ui::cmd_ui(port, no_open),
        Commands::Watch {
            agent,
            interval_ms,
            cycles,
        } => watch::cmd_watch(&agent, interval_ms, cycles),
        Commands::Gc { days, max_gb } => commands::cmd_gc(days, max_gb),
        Commands::Policy { action } => match action {
            PolicyAction::Init { force } => commands::cmd_policy_init(force),
            PolicyAction::List => commands::cmd_policy_list(),
            PolicyAction::Test { tool, target } => commands::cmd_policy_test(&tool, &target),
        },
        Commands::Doctor => commands::cmd_doctor(),
        Commands::Bench { files, iters, repo } => commands::cmd_bench(files, iters, repo),
        Commands::Completions { shell } => completions::print_completions(&shell),
    }
}
