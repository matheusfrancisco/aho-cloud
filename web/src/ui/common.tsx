import type { ReactNode } from "react";
import { ApiError } from "../lib/api";

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
