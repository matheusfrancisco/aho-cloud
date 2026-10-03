# Login banner for interactive shells in an aho cloud sandbox.
case "$-" in
*i*)
  if [ -z "${AHO_SANDBOX_MOTD_SHOWN:-}" ]; then
    export AHO_SANDBOX_MOTD_SHOWN=1
    printf 'aho cloud sandbox. Your files live in /workspace (kept across restarts).\n'
    printf 'Run `aho` for the agent, `aho --help` for its verbs.\n'
    if [ -z "${ANTHROPIC_API_KEY:-}${OPENAI_API_KEY:-}" ]; then
      printf 'No model key in the environment: add ANTHROPIC_API_KEY under Settings, Environment\n'
      printf '(then restart), or run `aho login anthropic` here.\n'
    fi
  fi
  ;;
esac
