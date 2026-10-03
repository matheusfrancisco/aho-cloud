// The Terminal tab: a login shell inside the sandbox, in xterm.js. The control plane mints a
// single-use socket token for the fitted size; the socket carries exec frames (`lib/exec.ts`) as
// binary messages. A session ends when the process exits, the server reports an error, or the
// socket drops; only the user reconnects.
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "../lib/api";
import { decodeFrame, encodeFrame, type Frame } from "../lib/exec";
import { Badge, ErrorBox } from "../ui/common";

type Phase = { kind: "connecting" } | { kind: "open" } | { kind: "exited"; code: number } | { kind: "failed"; error: unknown } | { kind: "closed"; code: number };

const OPEN = 1;

export function SandboxTerminal({ sandbox, active = true }: { sandbox: string; active?: boolean }) {
  const host = useRef<HTMLDivElement>(null);
  const xterm = useRef<{ term: Terminal; fit: FitAddon } | null>(null);
  const socket = useRef<WebSocket | null>(null);
  const [phase, setPhase] = useState<Phase>({ kind: "connecting" });

  const send = useCallback((f: Frame) => {
    const ws = socket.current;
    if (ws !== null && ws.readyState === OPEN) ws.send(encodeFrame(f));
  }, []);

  const connect = useCallback(async () => {
    const x = xterm.current;
    if (x === null) return;
    const old = socket.current;
    socket.current = null;
    old?.close(1000, "reconnecting");
    x.term.reset();
    setPhase({ kind: "connecting" });
    try {
      x.fit.fit();
      const { cols, rows } = x.term;
      const access = await api.sandboxTerminal(sandbox, { cols, rows, command: null, no_pty: false });
      // Unmounted, or another connect started, while the token was minted.
      if (xterm.current !== x || socket.current !== null) return;
      const ws = new WebSocket(`${access.url}${access.url.includes("?") ? "&" : "?"}token=${encodeURIComponent(access.token)}`);
      ws.binaryType = "arraybuffer";
      socket.current = ws;
      let ended = false;
      const end = (p: Phase) => {
        ended = true;
        setPhase(p);
      };
      ws.onopen = () => {
        if (socket.current !== ws) return;
        setPhase({ kind: "open" });
        // The terminal may have been resized while the token was minted.
        if (x.term.cols !== cols || x.term.rows !== rows) send({ kind: "resize", cols: x.term.cols, rows: x.term.rows });
        x.term.focus();
      };
      ws.onmessage = (ev: MessageEvent<unknown>) => {
        if (socket.current !== ws || typeof ev.data === "string") return;
        let f: Frame;
        try {
          f = decodeFrame(new Uint8Array(ev.data as ArrayBuffer));
        } catch (e) {
          end({ kind: "failed", error: e });
          ws.close(1000, "undecodable frame");
          return;
        }
        if (f.kind === "stdout" || f.kind === "stderr") x.term.write(f.data);
        else if (f.kind === "exit") end({ kind: "exited", code: f.code });
        else if (f.kind === "error") end({ kind: "failed", error: new Error(f.message) });
      };
      ws.onclose = (ev) => {
        if (socket.current !== ws) return;
        socket.current = null;
        if (!ended) setPhase({ kind: "closed", code: ev.code });
      };
    } catch (e) {
      if (xterm.current === x) setPhase({ kind: "failed", error: e });
    }
  }, [sandbox, send]);

  useEffect(() => {
    const el = host.current;
    if (el === null) return;
    const term = new Terminal({
      cursorBlink: true,
      fontFamily: 'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace',
      fontSize: 13,
      theme: { background: "#1f1b2d", foreground: "#ede9f7", cursor: "#c4b5fd", selectionBackground: "rgba(167, 139, 250, 0.35)" },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(el);
    const encoder = new TextEncoder();
    const subscriptions = [
      term.onData((d) => send({ kind: "stdin", data: encoder.encode(d) })),
      // Raw bytes (some mouse reports): one char per byte.
      term.onBinary((d) => send({ kind: "stdin", data: Uint8Array.from(d, (c) => c.charCodeAt(0) & 0xff) })),
      term.onResize(({ cols, rows }) => send({ kind: "resize", cols, rows })),
    ];
    xterm.current = { term, fit };
    void connect();
    return () => {
      xterm.current = null;
      const ws = socket.current;
      socket.current = null;
      ws?.close(1000, "terminal closed");
      for (const s of subscriptions) s.dispose();
      term.dispose();
    };
  }, [connect, send]);

  // Fit to the panel when it shows and whenever the window resizes; xterm reports the new size.
  useEffect(() => {
    if (!active) return;
    const fit = () => xterm.current?.fit.fit();
    fit();
    window.addEventListener("resize", fit);
    return () => window.removeEventListener("resize", fit);
  }, [active]);

  const ended = phase.kind === "exited" || phase.kind === "failed" || phase.kind === "closed";
  return (
    <div className="terminal-panel">
      <div className="terminal-bar">
        {phase.kind === "connecting" && <Badge tone="info">connecting</Badge>}
        {phase.kind === "open" && <Badge tone="ok">connected</Badge>}
        {phase.kind === "exited" && <code className="terminal-exit">[process exited with code {phase.code}]</code>}
        {phase.kind === "closed" && <span className="muted small">The connection closed ({phase.code}).</span>}
        {phase.kind === "failed" && (
          <div className="grow">
            <ErrorBox error={phase.error} />
          </div>
        )}
        <span className="grow" />
        {ended && (
          <button type="button" className="secondary small" onClick={() => void connect()}>
            Reconnect
          </button>
        )}
      </div>
      <div className="terminal-host" ref={host} />
    </div>
  );
}
