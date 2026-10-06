// Tickets list (server.md "UI"): identifier, title, status, top cause with confidence, target sha
// (`assumed` when it is the default-branch head), customer, last update. Filters live in the URL
// (`?status=&repo=&customer=&q=`); the org feed refreshes the list.
import { Ago, Badge, Loading, SearchIcon, useLoad } from "@aho/ui";
import { useEffect, useState } from "react";
import { Link, useSearchParams } from "react-router";
import { MageError, Sha } from "../components/common";
import { useFeed, useMage } from "../context";
import type { TicketListQuery, TicketSummary } from "../gen/mage";
import { formatConfidence } from "../lib/format";
import { TICKET_STATUS_LABEL, TICKET_STATUS_TONE, TICKET_STATUSES } from "../lib/labels";

const PAGE_SIZE = 50;

interface MorePages {
  /** The filters these pages belong to. */
  key: string;
  items: TicketSummary[];
  cursor: string | null;
}

export function TicketsPage() {
  const { api, link } = useMage();
  const [params, setParams] = useSearchParams();
  const rawStatus = params.get("status");
  const status = TICKET_STATUSES.find((s) => s === rawStatus);
  const repo = params.get("repo") ?? "";
  const customer = params.get("customer") ?? "";
  const q = params.get("q") ?? "";
  const query: TicketListQuery = { status, repo: repo || undefined, customer: customer || undefined, q: q || undefined, limit: PAGE_SIZE };
  const key = JSON.stringify(query);

  const first = useLoad(() => api.tickets(query), [api, key]);
  const repos = useLoad(() => api.repos(), [api]);
  const customers = useLoad(() => api.customers(), [api]);
  const [more, setMore] = useState<MorePages | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [moreError, setMoreError] = useState<unknown>(null);
  const [text, setText] = useState(q);
  useEffect(() => setText(q), [q]);

  useFeed(
    (e) => e.kind === "ticket" || e.kind === "cause",
    () => {
      setMore(null);
      first.reload();
    },
  );

  const setFilter = (name: string, value: string) => {
    const next = new URLSearchParams(params);
    if (value === "") next.delete(name);
    else next.set(name, value);
    setParams(next, { replace: true });
  };

  const extra = more !== null && more.key === key ? more : null;
  const rows = first.data === undefined ? [] : [...first.data.items, ...(extra?.items ?? [])];
  const cursor = extra !== null ? extra.cursor : (first.data?.next_cursor ?? null);
  const repoName = new Map((repos.data?.repos ?? []).map((r) => [r.repo.id, `${r.repo.owner}/${r.repo.name}`]));
  const customerName = new Map((customers.data?.customers ?? []).map((c) => [c.id, c.name]));
  const filtered = status !== undefined || repo !== "" || customer !== "" || q !== "";

  const loadMore = async () => {
    if (cursor === null) return;
    setLoadingMore(true);
    setMoreError(null);
    try {
      const page = await api.tickets({ ...query, cursor });
      setMore({ key, items: [...(extra?.items ?? []), ...page.items], cursor: page.next_cursor });
    } catch (e) {
      setMoreError(e);
    } finally {
      setLoadingMore(false);
    }
  };

  return (
    <div className="mage-page">
      <header className="mage-head">
        <h1>Tickets</h1>
      </header>
      <div className="mage-filters" role="group" aria-label="Filters">
        <label>
          Status
          <select value={status ?? ""} onChange={(e) => setFilter("status", e.target.value)}>
            <option value="">Any status</option>
            {TICKET_STATUSES.map((s) => (
              <option key={s} value={s}>
                {TICKET_STATUS_LABEL[s]}
              </option>
            ))}
          </select>
        </label>
        <label>
          Repo
          <select value={repo} onChange={(e) => setFilter("repo", e.target.value)}>
            <option value="">Any repo</option>
            {(repos.data?.repos ?? []).map((r) => (
              <option key={r.repo.id} value={r.repo.id}>
                {r.repo.owner}/{r.repo.name}
              </option>
            ))}
          </select>
        </label>
        <label>
          Customer
          <select value={customer} onChange={(e) => setFilter("customer", e.target.value)}>
            <option value="">Any customer</option>
            {(customers.data?.customers ?? []).map((c) => (
              <option key={c.id} value={c.id}>
                {c.name}
              </option>
            ))}
          </select>
        </label>
        <form
          className="search grow"
          role="search"
          onSubmit={(e) => {
            e.preventDefault();
            setFilter("q", text.trim());
          }}
        >
          <SearchIcon />
          <input type="search" aria-label="Search tickets" placeholder="Identifier, title or text" value={text} onChange={(e) => setText(e.target.value)} />
        </form>
        {filtered && (
          <button type="button" className="secondary" onClick={() => setParams(new URLSearchParams(), { replace: true })}>
            Clear filters
          </button>
        )}
      </div>
      <MageError error={first.error} />
      {first.data === undefined ? (
        first.loading && <Loading what="tickets" />
      ) : rows.length === 0 ? (
        <p className="muted">
          {filtered ? (
            "No tickets match these filters."
          ) : (
            <>
              No tickets yet. They arrive from Linear once the <Link to={link("settings/integrations")}>integration</Link> is connected.
            </>
          )}
        </p>
      ) : (
        <table aria-label="Tickets">
          <thead>
            <tr>
              <th>Ticket</th>
              <th>Title</th>
              <th>Status</th>
              <th>Top cause</th>
              <th>Target</th>
              <th>Customer</th>
              <th>Updated</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((t) => (
              <tr key={t.id}>
                <td className="mage-nowrap">
                  <Link to={link(`tickets/${t.id}`)}>{t.identifier}</Link>
                </td>
                <td>{t.title}</td>
                <td>
                  <Badge tone={TICKET_STATUS_TONE[t.status]}>{TICKET_STATUS_LABEL[t.status]}</Badge>
                </td>
                <td>
                  {t.top_cause === null ? (
                    <span className="muted">—</span>
                  ) : (
                    <>
                      {t.top_cause.title} <span className="muted small">{formatConfidence(t.top_cause.confidence)}</span>
                    </>
                  )}
                </td>
                <td>
                  {t.target === null ? (
                    <span className="muted">none</span>
                  ) : (
                    <span className="row small">
                      <span>{repoName.get(t.target.repo_id) ?? ""}</span>
                      <Sha sha={t.target.sha} />
                      {t.assumed && <Badge tone="warn">assumed</Badge>}
                    </span>
                  )}
                </td>
                <td>{t.customer === null ? <span className="muted">—</span> : (customerName.get(t.customer) ?? t.customer)}</td>
                <td>
                  <Ago ms={Date.parse(t.updated_at)} />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <MageError error={moreError} />
      {cursor !== null && (
        <button type="button" className="secondary" onClick={() => void loadMore()} disabled={loadingMore}>
          {loadingMore ? "Loading…" : "Load more"}
        </button>
      )}
    </div>
  );
}
