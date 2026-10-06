// Login (server.md "UI"): "Sign in with GitHub" and "Sign in with Google" are plain links to the
// server's OAuth entry points, which come back to `return_to`.
import { ErrorBox } from "@aho/ui";
import { Navigate, useSearchParams } from "react-router";
import { safeReturnTo, useSession } from "./session";

export function LoginPage() {
  const { api, session } = useSession();
  const [params] = useSearchParams();
  const returnTo = safeReturnTo(params.get("return_to"));
  const error = params.get("error");
  if (session !== undefined && session !== null) return <Navigate to={returnTo} replace />;
  return (
    <section className="card narrow stack">
      <h1>Sign in to Mage</h1>
      <p className="hint">Mage triages, reproduces and fixes support tickets against your code.</p>
      {error !== null && <ErrorBox error={`Sign-in failed (${error}). Try again.`} />}
      <a className="button" href={api.loginUrl("github", returnTo)}>
        Sign in with GitHub
      </a>
      <a className="button secondary" href={api.loginUrl("google", returnTo)}>
        Sign in with Google
      </a>
      <p className="hint">Signing in never gives Mage access to your repositories; an admin connects those separately.</p>
    </section>
  );
}
