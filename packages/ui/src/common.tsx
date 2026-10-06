import { type ReactNode, useState } from "react";
import { ApiError } from "../lib/api";
import type { StepState } from "../lib/golive";
import { CheckIcon, CopyIcon } from "./icons";

/** An error as the user should read it: API code and refusals, or the plain message. */
export function ErrorBox({ error }: { error: unknown }) {
  if (error === null || error === undefined) return null;
  if (error instanceof ApiError) {
    return (
      <div className="error" role="alert">
        <strong>{error.code}</strong>: {error.message}
        {error.refusals.length > 0 && (
          <ul>
            {error.refusals.map((r) => (
              <li key={`${r.code}:${r.field}`}>
                <code>{r.field}</code> {r.message} <em>Fix: {r.fix}</em>
              </li>
            ))}
          </ul>
        )}
      </div>
    );
  }
  const message = error instanceof Error ? error.message : String(error);
  return (
    <div className="error" role="alert">
      {message}
    </div>
  );
}

/** A small state label; `tone` picks the color. */
export function Badge({ tone, children }: { tone: "ok" | "warn" | "bad" | "info" | "muted"; children: ReactNode }) {
  return <span className={`badge badge-${tone}`}>{children}</span>;
}

export function Loading({ what }: { what: string }) {
  return <p className="muted">Loading {what}…</p>;
}

/** `blake3:9f2c…` / `sha256:…` shortened for tables; the full value stays in the title. */
export function Hash({ value }: { value: string }) {
  const [algo, hex] = value.includes(":") ? value.split(":", 2) : ["", value];
  return (
    <code className="hash" title={value}>
      {algo === "" ? "" : `${algo}:`}
      {(hex ?? "").slice(0, 12)}
    </code>
  );
}

export function Time({ ms }: { ms: number }) {
  const d = new Date(ms);
  return <time dateTime={d.toISOString()}>{d.toLocaleString()}</time>;
}

const UNITS = ["B", "KB", "MB", "GB"] as const;

/** `1536` → `1.5 KB`. */
export function formatBytes(n: number): string {
  let v = n;
  let u = 0;
  while (v >= 1024 && u < UNITS.length - 1) {
    v /= 1024;
    u++;
  }
  return `${u === 0 ? v : v.toFixed(v < 10 ? 1 : 0)} ${UNITS[u]}`;
}

const RELATIVE = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });

/** "3 minutes ago", with the exact time on hover. */
export function Ago({ ms, now = Date.now() }: { ms: number; now?: number }) {
  const s = Math.round((ms - now) / 1000);
  const abs = Math.abs(s);
  const text =
    abs < 60 ? RELATIVE.format(s, "second") : abs < 3600 ? RELATIVE.format(Math.round(s / 60), "minute") : abs < 86_400 ? RELATIVE.format(Math.round(s / 3600), "hour") : RELATIVE.format(Math.round(s / 86_400), "day");
  const d = new Date(ms);
  return (
    <time dateTime={d.toISOString()} title={d.toLocaleString()}>
      {text}
    </time>
  );
}

/** Copies `text` to the clipboard and says so for a moment. */
export function CopyButton({ text, label = "Copy" }: { text: string; label?: string }) {
  const [copied, setCopied] = useState(false);
  const [failed, setFailed] = useState(false);
  return (
    <button
      type="button"
      className="icon-button copy"
      aria-label={copied ? "Copied" : label}
      title={failed ? "The clipboard is not available; select the text instead." : label}
      onClick={() => {
        // `navigator.clipboard` is missing outside secure contexts (plain http on a LAN host).
        (navigator.clipboard === undefined ? Promise.reject(new Error("no clipboard")) : navigator.clipboard.writeText(text)).then(
          () => {
            setCopied(true);
            setFailed(false);
            setTimeout(() => setCopied(false), 1500);
          },
          () => setFailed(true),
        );
      }}
    >
      {copied ? <CheckIcon /> : <CopyIcon />}
    </button>
  );
}

/** A vertical checklist of a multi-step operation. */
export function StepList({ steps }: { steps: ReadonlyArray<{ id: string; label: ReactNode; state: StepState; detail?: ReactNode }> }) {
  return (
    <ol className="steps">
      {steps.map((s) => (
        <li key={s.id} className={`step step-${s.state}`} data-step={s.id} data-state={s.state}>
          <span className="step-mark" aria-hidden="true" />
          <span className="step-label">
            {s.label}
            {s.state === "skipped" && <span className="muted small"> — not needed</span>}
            {s.detail !== undefined && <span className="step-detail">{s.detail}</span>}
          </span>
        </li>
      ))}
    </ol>
  );
}
