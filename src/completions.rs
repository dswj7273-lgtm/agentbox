use anyhow::bail;

pub fn print_completions(shell: &str) -> anyhow::Result<()> {
    match shell {
        "bash" => print!("{}", BASH_COMPLETIONS),
        "zsh" => print!("{}", ZSH_COMPLETIONS),
        "fish" => print!("{}", FISH_COMPLETIONS),
        other => bail!("Unsupported shell `{}` (expected bash, zsh, or fish)", other),
    }
    Ok(())
}

const BASH_COMPLETIONS: &str = r#"_abox_completions() {
    local cur prev cmds
    COMPREPLY=()
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"
    cmds="init uninstall hook log show diff undo checkpoint status projects report cost export ui watch policy gc doctor bench completions"

    if [[ ${COMP_CWORD} -eq 1 ]]; then
        COMPREPLY=( $(compgen -W "${cmds}" -- "${cur}") )
        return 0
    fi

    case "${prev}" in
        policy)
            COMPREPLY=( $(compgen -W "init list test" -- "${cur}") )
            return 0
            ;;
        completions)
            COMPREPLY=( $(compgen -W "bash zsh fish" -- "${cur}") )
            return 0
            ;;
        --format)
            COMPREPLY=( $(compgen -W "jsonl otel markdown" -- "${cur}") )
            return 0
            ;;
        --by)
            COMPREPLY=( $(compgen -W "branch session agent" -- "${cur}") )
            return 0
            ;;
        --agent)
            COMPREPLY=( $(compgen -W "claude codex gemini opencode cursor abox" -- "${cur}") )
            return 0
            ;;
    esac
}
complete -F _abox_completions abox
"#;

const ZSH_COMPLETIONS: &str = r#"#compdef abox

_abox() {
  local -a commands
  commands=(
    'init:Detect AI coding agents, install hooks, and initialize shadow repo'
    'uninstall:Remove AgentBox hooks cleanly'
    'hook:Internal hook entrypoint invoked by AI coding agents'
    'log:Show step timeline for the current project'
    'show:Show detailed metadata and changed files for a step'
    'diff:Show diff for a step, range, or cp:<tag>'
    'undo:Undo agent changes safely'
    'checkpoint:Save or list named workspace checkpoints'
    'status:Show AgentBox hook and workspace status'
    'projects:List all tracked repositories'
    'report:Scan historical local agent logs'
    'cost:Summarize token and USD cost'
    'export:Export audit timeline (jsonl, otel, markdown)'
    'ui:Launch local Web UI'
    'watch:Watch filesystem changes for hookless editors'
    'policy:Inspect, initialize, or test Guard policy rules'
    'gc:Prune old sessions and compact shadow repos'
    'doctor:Check installation, git, hooks, and DB integrity'
    'bench:Run shadow-git snapshot latency benchmark'
    'completions:Generate shell completion script'
  )

  _arguments -C \
    '1: :->cmds' \
    '*::arg:->args'

  case "$state" in
    cmds)
      _describe 'command' commands
      ;;
  esac
}

_abox "$@"
"#;

const FISH_COMPLETIONS: &str = r#"complete -c abox -f
for cmd in init uninstall hook log show diff undo checkpoint status projects report cost export ui watch policy gc doctor bench completions
    complete -c abox -n "__fish_use_subcommand" -a "$cmd"
end
complete -c abox -n "__fish_seen_subcommand_from policy" -a "init list test"
complete -c abox -n "__fish_seen_subcommand_from completions" -a "bash zsh fish"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_completions_for_supported_shells() {
        assert!(print_completions("bash").is_ok());
        assert!(print_completions("zsh").is_ok());
        assert!(print_completions("fish").is_ok());
        assert!(print_completions("pwsh").is_err());
    }
}
