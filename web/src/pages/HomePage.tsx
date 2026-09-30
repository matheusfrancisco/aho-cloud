// `/`: the org the user last worked in (or their first); a first-run card when they have none.
import { Navigate, useNavigate } from "react-router";
import { NewOrgForm, useShell } from "../components/Shell";
import { currentOrg, setCurrentOrg } from "../lib/remembered";
import { ErrorBox, Loading } from "../ui/common";
import { EmptyIllustration } from "../ui/icons";

export function HomePage() {
  const { me, meError, reloadMe } = useShell();
  const navigate = useNavigate();
  if (me === undefined) return meError !== null ? <ErrorBox error={meError} /> : <Loading what="your organizations" />;
  const remembered = currentOrg();
  const target = me.orgs.find((m) => m.org === remembered)?.org ?? me.orgs[0]?.org;
  if (target !== undefined) return <Navigate to={`/orgs/${encodeURIComponent(target)}`} replace />;
  return (
    <section className="card welcome">
      <EmptyIllustration />
      <h1>Welcome to aho cloud</h1>
      <p className="hint">
        Projects live in an organization. Create one to start, or ask an owner to add <strong>{me.user.email}</strong> to theirs.
      </p>
      <NewOrgForm
        onCreated={(slug) => {
          reloadMe();
          setCurrentOrg(slug);
          navigate(`/orgs/${encodeURIComponent(slug)}`);
        }}
      />
    </section>
  );
}
