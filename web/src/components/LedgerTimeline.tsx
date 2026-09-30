// The project's release ledger, newest first, one line per fact.
import type { ReactNode } from "react";
import { Link } from "react-router";
import type { LedgerEntry } from "../gen/api";
import type { Approver, LedgerBody, OnCallAction, StandingOrder } from "../gen/contract";
import { Badge, Hash, Time } from "../ui/common";

function orderText(o: StandingOrder): string {
  return o.kind === "scale" ? `scale ${o.min}–${o.max}` : o.kind.replaceAll("_", " ");
}

function actionText(a: OnCallAction): string {
  switch (a.kind) {
    case "restart":
      return "restart";
    case "scale":
      return `scale to ${a.replicas}`;
    case "maintenance_page":
      return `maintenance page ${a.on ? "on" : "off"}`;
    case "rollback":
      return a.fits_within_ceiling ? "rollback within ceiling" : "rollback";
  }
}

function signer(a: Approver): ReactNode {
  return (
    <>
      {a.email}
      {a.owner && " (owner)"} via {a.method.replace("_", " ")}
    </>
  );
}

/** One ledger fact as a line of text, release ids linked to their approval cards. */
export function describeEntry(project: string, body: LedgerBody): ReactNode {
  const rel = (r: string) => (
    <Link to={`/projects/${encodeURIComponent(project)}/releases/${encodeURIComponent(r)}`}>
      <code>{r}</code>
    </Link>
  );
  switch (body.kind) {
    case "release_proposed": {
      const p = body.proposer;
      const by = p.kind === "agent" ? `agent (session ${p.session_id})` : p.kind === "ci" ? `CI ${p.name}` : `user ${p.user_id}`;
      return (
        <>
          {rel(body.release)} proposed by {by}, {body.provenance.kind}
        </>
      );
    }
    case "image_built":
      return (
        <>
          {rel(body.release)} built: <Hash value={body.image_digest} />
        </>
      );
    case "surface_observed":
      return (
        <>
          {rel(body.release)} observed listening on {body.listeners.join(", ") || "nothing"}; egress {body.egress.join(", ") || "none"}
        </>
      );
    case "taint_checked":
      return body.findings.length === 0 ? (
        <>{rel(body.release)} taint check clean</>
      ) : (
        <>
          {rel(body.release)} tainted: {body.findings.map((f) => f.secret).join(", ")}
        </>
      );
    case "secret_rotated":
      return (
        <>
          secret <code>{body.secret}</code> rotated by {body.by}
        </>
      );
    case "approved":
      return (
        <>
          {rel(body.release)} approved for {body.stage} by {signer(body.approver)}
        </>
      );
    case "rejected":
      return (
        <>
          {rel(body.release)} rejected for {body.stage} by {body.user_id}: {body.reason}
        </>
      );
    case "activated":
      return (
        <>
          {rel(body.release)} active at {body.stage} ({body.reason.replace("_", " ")})
        </>
      );
    case "standing_orders_granted":
      return (
        <>
          standing orders granted ({body.orders.map(orderText).join(", ")}) until <Time ms={body.expires_at * 1000} /> by {signer(body.approver)}
        </>
      );
    case "standing_order_executed":
      return (
        <>
          {body.actor} ran {actionText(body.action)}
          {body.release !== null && <> to {rel(body.release)}</>}
        </>
      );
    case "policy_changed":
      return (
        <>
          policy changed: {body.policy.non_widening} approval(s) for non-widening, {body.policy.widening} for widening, owner for {body.policy.owner_only.join(", ") || "nothing"} by{" "}
          {signer(body.approver)}
        </>
      );
    case "members_changed":
      return (
        <>
          organization now has {body.count} member{body.count === 1 ? "" : "s"}; approvals are capped at that count
        </>
      );
  }
}

export function LedgerTimeline({ project, entries }: { project: string; entries: LedgerEntry[] }) {
  if (entries.length === 0) return <p className="muted">The ledger is empty.</p>;
  const newestFirst = [...entries].sort((a, b) => b.id - a.id);
  return (
    <ol className="timeline">
      {newestFirst.map((e) => (
        <li key={e.id}>
          <div className="row">
            <Badge tone="muted">{e.body.kind.replaceAll("_", " ")}</Badge>
            <Time ms={e.at} />
            <Hash value={e.hash} />
          </div>
          <div>{describeEntry(project, e.body)}</div>
        </li>
      ))}
    </ol>
  );
}
