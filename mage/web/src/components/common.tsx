// Small pieces the Mage pages share.
import { Badge, ErrorBox } from "@aho/ui";
import { type ReactNode, useId } from "react";
import { MageApiError } from "../api/client";
import type { RunStatus } from "../gen/mage";
import { RUN_STATUS_LABEL, RUN_STATUS_TONE } from "../lib/labels";
import { safeHref } from "../lib/markdown";

/** `ErrorBox` plus the server's request id, so support can find the request. */
export function MageError({ error }: { error: unknown }) {
  if (error === null || error === undefined) return null;
  return (
    <>
      <ErrorBox error={error} />
      {error instanceof MageApiError && error.requestId !== null && (
        <p className="muted small">
          Request id <code>{error.requestId}</code>
        </p>
      )}
    </>
  );
}

/** A commit, shortened; the full sha is in the title. */
export function Sha({ sha }: { sha: string }) {
  return (
    <code className="hash" title={sha}>
      {sha.slice(0, 7)}
    </code>
  );
}

export function RunStatusBadge({ status }: { status: RunStatus }) {
  return <Badge tone={RUN_STATUS_TONE[status]}>{RUN_STATUS_LABEL[status]}</Badge>;
}

/** An `aria`-wired tab strip over `tabs`; the panel shows `children` for the selected tab. */
export function Tabs<T extends string>({ label, tabs, selected, onSelect, children }: { label: string; tabs: ReadonlyArray<{ id: T; label: ReactNode }>; selected: T; onSelect: (tab: T) => void; children: ReactNode }) {
  const prefix = useId();
  return (
    <>
      <div className="tabs" role="tablist" aria-label={label}>
        {tabs.map((t) => (
          <button key={t.id} type="button" role="tab" id={`${prefix}-tab-${t.id}`} aria-controls={`${prefix}-panel`} aria-selected={selected === t.id} className={`tab${selected === t.id ? " active" : ""}`} onClick={() => onSelect(t.id)}>
            {t.label}
          </button>
        ))}
      </div>
      <div className="tab-panel" role="tabpanel" id={`${prefix}-panel`} aria-labelledby={`${prefix}-tab-${selected}`}>
        {children}
      </div>
    </>
  );
}

/**
 * A link to another site: absolute http(s) URLs only (others render as plain text), opened without
 * a referrer or a handle on this window.
 */
export function ExternalLink({ href, children }: { href: string; children: ReactNode }) {
  const safe = safeHref(href);
  if (safe === null) return <span>{children}</span>;
  return (
    <a href={safe} target="_blank" rel="noopener noreferrer">
      {children}
    </a>
  );
}
