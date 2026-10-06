// Routes of the standalone shell: `/login` (signed out), `/` (org picker), `/o/{org}/**` (Mage
// pages from @mage/web), and the short links used in Linear, Slack and fix briefs.
import { ErrorBox, Loading, useLoad } from "@aho/ui";
import { type EventSourceFactory, MageError, MageProvider, MageRoutes, type ResolvedKind } from "@mage/web";
import { useEffect } from "react";
import { Link, Navigate, Outlet, Route, Routes, useLocation, useParams } from "react-router";
import { Frame, PublicFrame } from "./Frame";
import { LoginPage } from "./LoginPage";
import { rememberedOrg, rememberOrg, SessionProvider, useSession } from "./session";

/** Signed-out visitors go to the login page and come back afterwards. */
function RequireSession() {
  const { session, error, reload, didLogOut } = useSession();
  const location = useLocation();
  if (session === undefined) {
    return error === null ? (
      <PublicFrame>
        <Loading what="your session" />
      </PublicFrame>
    ) : (
      <PublicFrame>
        <MageError error={error} />
        <button type="button" onClick={reload}>
          Try again
        </button>
      </PublicFrame>
    );
  }
  if (session === null) return <Navigate to={didLogOut ? "/login" : `/login?return_to=${encodeURIComponent(location.pathname + location.search + location.hash)}`} replace />;
  return <Outlet />;
}

/** `/`: the remembered org, the only org, or a choice. */
function OrgPicker() {
  const { session } = useSession();
  if (session === undefined || session === null) return null;
  const memberships = session.memberships;
  const remembered = rememberedOrg(session.user.id);
  const target = memberships.find((m) => m.org === remembered) ?? (memberships.length === 1 ? memberships[0] : undefined);
  if (target !== undefined) return <Navigate to={`/o/${encodeURIComponent(target.org)}/tickets`} replace />;
  return (
    <Frame org={null}>
      {memberships.length === 0 ? (
        <section className="card narrow stack">
          <h1>No access yet</h1>
          <p>You are signed in as {session.user.email}, but you are not a member of any organization.</p>
          <p className="hint">Ask an admin of your organization to invite your GitHub login; the invite applies the next time you sign in.</p>
        </section>
      ) : (
        <section className="card narrow stack">
          <h1>Choose an organization</h1>
          <ul>
            {memberships.map((m) => (
              <li key={m.org}>
                <Link to={`/o/${encodeURIComponent(m.org)}/tickets`}>{m.name}</Link> <span className="muted small">{m.role}</span>
              </li>
            ))}
          </ul>
        </section>
      )}
    </Frame>
  );
}

/** `/o/{org}/**`: the Mage pages for a membership (another org looks like a missing page). */
function OrgPages() {
  const { session, fetch, signedOut, eventSource } = useSession();
  const slug = useParams().org ?? "";
  const membership = session?.memberships.find((m) => m.org === slug) ?? null;
  const userId = session?.user.id;
  useEffect(() => {
    if (membership !== null && userId !== undefined) rememberOrg(userId, membership.org);
  }, [membership, userId]);
  if (session === undefined || session === null) return null;
  if (membership === null) {
    return (
      <Frame org={null}>
        <p className="muted">Not found.</p>
      </Frame>
    );
  }
  return (
    <Frame org={membership}>
      <MageProvider host={{ apiBase: "", user: session.user, org: membership, fetch, basePath: `/o/${encodeURIComponent(membership.org)}`, csrfToken: session.csrf_token, onUnauthorized: signedOut, eventSource }}>
        <MageRoutes />
      </MageProvider>
    </Frame>
  );
}

/** Short links (`/tickets/{id}`, `/runs/{id}`, `/causes/{id}/brief`, `/artifacts/{id}`) → the org page. */
function ShortLink({ kind }: { kind: ResolvedKind }) {
  const { api } = useSession();
  const id = useParams().id ?? "";
  const target = useLoad(async () => {
    const owner = await api.resolve(id);
    const base = `/o/${encodeURIComponent(owner.org)}`;
    const ref = encodeURIComponent(id);
    switch (kind) {
      case "ticket":
        return `${base}/tickets/${ref}`;
      case "run":
        return `${base}/runs/${ref}`;
      case "cause":
        if (owner.ticket === undefined) throw new Error("this cause has no ticket");
        return `${base}/tickets/${encodeURIComponent(owner.ticket)}?cause=${ref}&tab=brief`;
      case "artifact": {
        const artifact = await api.org(owner.org).artifact(id);
        return `${base}/runs/${encodeURIComponent(artifact.run_id)}#artifact-${ref}`;
      }
    }
  }, [api, id, kind]);
  if (target.data !== undefined) return <Navigate to={target.data} replace />;
  return <Frame org={null}>{target.loading ? <Loading what="the link" /> : <ErrorBox error={target.error} />}</Frame>;
}

export function App({ fetch = (input, init) => globalThis.fetch(input, init), eventSource }: { fetch?: typeof globalThis.fetch; eventSource?: EventSourceFactory }) {
  return (
    <SessionProvider fetch={fetch} eventSource={eventSource}>
      <Routes>
        <Route
          path="/login"
          element={
            <PublicFrame>
              <LoginPage />
            </PublicFrame>
          }
        />
        <Route element={<RequireSession />}>
          <Route path="/" element={<OrgPicker />} />
          <Route path="/o/:org/*" element={<OrgPages />} />
          <Route path="/tickets/:id" element={<ShortLink kind="ticket" />} />
          <Route path="/runs/:id" element={<ShortLink kind="run" />} />
          <Route path="/causes/:id/brief" element={<ShortLink kind="cause" />} />
          <Route path="/artifacts/:id" element={<ShortLink kind="artifact" />} />
          <Route
            path="*"
            element={
              <Frame org={null}>
                <p className="muted">Not found.</p>
              </Frame>
            }
          />
        </Route>
      </Routes>
    </SessionProvider>
  );
}
