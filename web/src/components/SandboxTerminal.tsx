// The Terminal tab: a login shell inside the sandbox, in `@aho/ui`'s `Terminal`. The control plane
// mints a single-use socket token for the fitted size; the socket carries exec frames.
import { type ExecSocketFactory, Terminal } from "@aho/ui/terminal";
import { api } from "../lib/api";

function loginShell(sandbox: string): ExecSocketFactory {
  return {
    id: sandbox,
    prepare: async ({ cols, rows }) => {
      const access = await api.sandboxTerminal(sandbox, { cols, rows, command: null, no_pty: false });
      return () => new WebSocket(`${access.url}${access.url.includes("?") ? "&" : "?"}token=${encodeURIComponent(access.token)}`);
    },
  };
}

export function SandboxTerminal({ sandbox, active = true }: { sandbox: string; active?: boolean }) {
  return <Terminal sockets={loginShell(sandbox)} active={active} />;
}
