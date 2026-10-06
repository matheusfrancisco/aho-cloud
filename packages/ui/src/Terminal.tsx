// An interactive terminal in xterm.js over an exec socket: the socket carries exec frames
// (`exec.ts`) as binary messages. A session ends when the process exits, the server reports an
// error, or the socket drops; only the user reconnects. Exported from `@aho/ui/terminal` so
// xterm loads only where a terminal is shown.
import { FitAddon } from "@xterm/addon-fit";
import { Terminal as XTerm } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { useCallback, useEffect, useRef, useState } from "react";
import { Badge, ErrorBox } from "./common";
import { decodeFrame, encodeFrame, type Frame, type PtySize } from "./exec";

/** Where a `Terminal` gets its sessions. */
export interface ExecSocketFactory {
  /** Identifies the session target; a change starts a new session. */
  readonly id: string;
  /**
   * Prepares a session for a terminal of `size` (e.g. mints a single-use socket token) and
   * returns a function that opens its socket. Called on every (re)connect; the returned function
   * is not called when the terminal unmounted or reconnected meanwhile.
   */
  prepare(size: PtySize): Promise<() => WebSocket>;
}

type Phase = { kind: "connecting" } | { kind: "open" } | { kind: "exited"; code: number } | { kind: "failed"; error: unknown } | { kind: "closed"; code: number };

const OPEN = 1;

/** `active`: the terminal is visible; it refits to its panel when it becomes so and on window resizes. */
export function Terminal({ sockets, active = true }: { sockets: ExecSocketFactory; active?: boolean }) {
  const host = useRef<HTMLDivElement>(null);
  const xterm = useRef<{ term: XTerm; fit: FitAddon } | null>(null);
  const socket = useRef<WebSocket | null>(null);
  const [phase, setPhase] = useState<Phase>({ kind: "connecting" });
  // Read through a ref: a new factory object with the same `id` keeps the session.
  const factory = useRef(sockets);
  factory.current = sockets;
  const target = sockets.id;

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
      const open = await factory.current.prepare({ cols, rows });
      // Unmounted, or another connect started, while the session was prepared.
      if (xterm.current !== x || socket.current !== null) return;
      const ws = open();
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
        // The terminal may have been resized while the session was prepared.
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
    // `target` identifies the session; the factory itself is read through a ref.
  }, [target, send]);

  useEffect(() => {
    const el = host.current;
    if (el === null) return;
    const term = new XTerm({
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
