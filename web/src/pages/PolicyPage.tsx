// The org's release policy: how many approvals staging and live need, and which widenings an
// owner must sign. Read-only here; changing it is a signed request.
import { useEffect } from "react";
import { useParams } from "react-router";
import { ErrorBox, Loading, useLoad } from "@aho/ui";
import { api } from "../lib/api";
import { setCurrentOrg } from "../lib/remembered";

export function PolicyPage() {
  const org = useParams().org ?? "";
  useEffect(() => setCurrentOrg(org), [org]);
  const loaded = useLoad(async () => {
    const [policy, members] = await Promise.all([api.policy(org), api.members(org)]);
    return { policy, members: members.length };
  }, [org]);

  return (
    <section className="card page-card">
      <h1>Release policy</h1>
      <p className="hint">Preview needs no approval. Promoting a release to staging and to live needs passkey or device-key approvals from org members.</p>
      <ErrorBox error={loaded.error} />
      {loaded.data === undefined ? (
        loaded.loading && <Loading what="policy" />
      ) : (
        <>
          <dl className="policy">
            <dt>Releases that widen nothing</dt>
            <dd>
              <strong>{loaded.data.policy.non_widening}</strong> approval{loaded.data.policy.non_widening === 1 ? "" : "s"} per stage
            </dd>
            <dt>Releases that widen the surface</dt>
            <dd>
              <strong>{loaded.data.policy.widening}</strong> approval{loaded.data.policy.widening === 1 ? "" : "s"} per stage
            </dd>
            <dt>Owner must approve widening of</dt>
            <dd>
              {loaded.data.policy.owner_only.length === 0 ? (
                <span className="muted">nothing</span>
              ) : (
                <span className="tags">
                  {loaded.data.policy.owner_only.map((c) => (
                    <span key={c} className="tag">
                      {c.replaceAll("_", " ")}
                    </span>
                  ))}
                </span>
              )}
            </dd>
          </dl>
          <p className="hint">
            With {loaded.data.members} member{loaded.data.members === 1 ? "" : "s"}, a release needs at most {loaded.data.members} approval{loaded.data.members === 1 ? "" : "s"}; an owner is still
            required where the policy says so.
          </p>
        </>
      )}
    </section>
  );
}
