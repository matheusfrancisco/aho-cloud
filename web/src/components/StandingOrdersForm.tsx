// Pre-approves bounded on-call verbs for a while. The grant itself is signed with a passkey.
import { useState } from "react";
import type { StandingOrder } from "../gen/contract";
import { api } from "../lib/api";
import { standingOrdersMessage } from "../lib/approval";
import { currentSession } from "../lib/session";
import { passkeyProof } from "../lib/webauthn";
import { ErrorBox } from "../ui/common";
import { useAction } from "../ui/hooks";

export function StandingOrdersForm({ project }: { project: string }) {
  const [restart, setRestart] = useState(true);
  const [scale, setScale] = useState(false);
  const [min, setMin] = useState(1);
  const [max, setMax] = useState(3);
  const [maintenance, setMaintenance] = useState(true);
  const [rollback, setRollback] = useState(true);
  const [hours, setHours] = useState(24 * 7);
  const [done, setDone] = useState<string | null>(null);

  const orders: StandingOrder[] = [];
  if (restart) orders.push({ kind: "restart" });
  if (scale) orders.push({ kind: "scale", min, max });
  if (maintenance) orders.push({ kind: "maintenance_page" });
  if (rollback) orders.push({ kind: "rollback_within_ceiling" });
  const scaleValid = !scale || (Number.isInteger(min) && Number.isInteger(max) && min >= 0 && min <= max);

  const grant = useAction(async () => {
    const session = currentSession();
    if (session === null) throw new Error("Sign in to grant standing orders.");
    const proof = await passkeyProof(api, session.user.email, standingOrdersMessage(project, orders));
    await api.grantStandingOrders(project, { orders, ttl_secs: Math.round(hours * 3600), proof });
    setDone(`Granted for ${hours} h.`);
  });

  return (
    <form
      className="standing-orders"
      onSubmit={(e) => {
        e.preventDefault();
        setDone(null);
        void grant.run();
      }}
    >
      <p className="hint">On-call (human or agent) may do these without asking until the grant expires. Every use is recorded in the ledger.</p>
      <label className="check">
        <input type="checkbox" checked={restart} onChange={(e) => setRestart(e.target.checked)} /> Restart
      </label>
      <label className="check">
        <input type="checkbox" checked={scale} onChange={(e) => setScale(e.target.checked)} /> Scale replicas within
        <input type="number" min={0} value={min} onChange={(e) => setMin(e.target.valueAsNumber)} disabled={!scale} aria-label="minimum replicas" />–
        <input type="number" min={0} value={max} onChange={(e) => setMax(e.target.valueAsNumber)} disabled={!scale} aria-label="maximum replicas" />
      </label>
      <label className="check">
        <input type="checkbox" checked={maintenance} onChange={(e) => setMaintenance(e.target.checked)} /> Maintenance page
      </label>
      <label className="check">
        <input type="checkbox" checked={rollback} onChange={(e) => setRollback(e.target.checked)} /> Roll back to a release that fits within live's surface
      </label>
      <label>
        Valid for (hours)
        <input type="number" min={1} value={hours} onChange={(e) => setHours(e.target.valueAsNumber)} />
      </label>
      <div className="row">
        <button type="submit" disabled={grant.busy || orders.length === 0 || !scaleValid || !(hours >= 1)}>
          {grant.busy ? "Waiting for passkey…" : "Grant with passkey"}
        </button>
        {done !== null && <span className="ok">{done}</span>}
      </div>
      <ErrorBox error={grant.error} />
    </form>
  );
}
