#!/bin/sh
# `aho` in a sandbox: the conventional provider variables (set in the console under Settings,
# Environment) become aho's own settings, `provider.<name>.key` = AHO_PROVIDER_<NAME>_KEY, unless
# those are set already. A home whose `~/.aho/config.toml` was removed gets the image's default
# back. Then the real binary.
if [ -n "${ANTHROPIC_API_KEY:-}" ] && [ -z "${AHO_PROVIDER_ANTHROPIC_KEY:-}" ]; then
  export AHO_PROVIDER_ANTHROPIC_KEY="$ANTHROPIC_API_KEY"
fi
if [ -n "${OPENAI_API_KEY:-}" ] && [ -z "${AHO_PROVIDER_OPENAI_KEY:-}" ]; then
  export AHO_PROVIDER_OPENAI_KEY="$OPENAI_API_KEY"
fi
if [ ! -e "$HOME/.aho/config.toml" ]; then
  mkdir -p -m 0700 "$HOME/.aho" && cp /etc/aho/config.toml "$HOME/.aho/config.toml"
fi
exec /usr/local/libexec/aho "$@"
