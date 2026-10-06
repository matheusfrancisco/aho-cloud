// Settings → Usage (Admin): metered model cost in `[from, to)`, by run, ticket or model.
import { Loading, useLoad } from "@aho/ui";
import { useState } from "react";
import { MageError } from "../../components/common";
import { useMage } from "../../context";
import type { UsageGroupBy } from "../../gen/mage";
import { formatCost } from "../../lib/format";

const DAY_MS = 86_400_000;

const GROUPS: ReadonlyArray<readonly [UsageGroupBy, string]> = [
  ["ticket", "Ticket"],
  ["run", "Run"],
  ["model", "Model"],
];

export function UsageSettings() {
  const { api, can } = useMage();
  const admin = can("admin");
  // `YYYY-MM-DD` (UTC): the last 30 days, today included.
  const [from, setFrom] = useState(() => new Date(Date.now() - 30 * DAY_MS).toISOString().slice(0, 10));
  const [to, setTo] = useState(() => new Date(Date.now() + DAY_MS).toISOString().slice(0, 10));
  const [groupBy, setGroupBy] = useState<UsageGroupBy>("ticket");
  const valid = from !== "" && to !== "" && from < to;
  const report = useLoad(() => (admin && valid ? api.usage({ from: `${from}T00:00:00Z`, to: `${to}T00:00:00Z`, group_by: groupBy }) : Promise.resolve(null)), [api, admin, valid, from, to, groupBy]);
  if (!admin) return <p className="muted">Only admins see usage.</p>;
  return (
    <div className="stack">
      <div className="row">
        <label>
          From
          <input type="date" value={from} onChange={(e) => setFrom(e.target.value)} />
        </label>
        <label>
          To (exclusive)
          <input type="date" value={to} onChange={(e) => setTo(e.target.value)} />
        </label>
        <label>
          Group by
          <select value={groupBy} onChange={(e) => setGroupBy(GROUPS.find(([g]) => g === e.target.value)?.[0] ?? groupBy)}>
            {GROUPS.map(([g, label]) => (
              <option key={g} value={g}>
                {label}
              </option>
            ))}
          </select>
        </label>
      </div>
      {!valid && <p className="field-error">The start must be before the end.</p>}
      <MageError error={report.error} />
      {report.data === undefined ? (
        report.loading && <Loading what="usage" />
      ) : report.data === null ? null : (
        <>
          <p>
            Total: <strong>{formatCost(report.data.total_cost_micros)}</strong>
          </p>
          {report.data.rows.length === 0 ? (
            <p className="muted">No metered usage in this period.</p>
          ) : (
            <table aria-label="Usage">
              <thead>
                <tr>
                  <th>{GROUPS.find(([g]) => g === report.data?.group_by)?.[1] ?? "Key"}</th>
                  <th>Runs</th>
                  <th>Input tokens</th>
                  <th>Output tokens</th>
                  <th>Cost</th>
                </tr>
              </thead>
              <tbody>
                {report.data.rows.map((r) => (
                  <tr key={r.key}>
                    <td>
                      {r.label ?? <code>{r.key}</code>}
                      {r.label !== null && <div className="muted small">{r.key}</div>}
                    </td>
                    <td>{r.runs}</td>
                    <td>{r.input_tokens.toLocaleString()}</td>
                    <td>{r.output_tokens.toLocaleString()}</td>
                    <td>{formatCost(r.cost_micros)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </>
      )}
    </div>
  );
}
