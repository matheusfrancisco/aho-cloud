import { useState } from "react";
import { Navigate, useNavigate, useSearchParams } from "react-router";
import { api } from "../lib/api";
import { useSession } from "../lib/session";
import { loginPasskey, registerPasskey } from "../lib/webauthn";
import { ErrorBox } from "../ui/common";
import { useAction } from "../ui/hooks";

/** Only same-app paths are followed after login. */
function safeNext(next: string | null): string {
  return next !== null && next.startsWith("/") && !next.startsWith("//") ? next : "/";
}

export function LoginPage() {
  const session = useSession();
  const navigate = useNavigate();
  const [params] = useSearchParams();
  const next = safeNext(params.get("next"));
  const [mode, setMode] = useState<"login" | "register">("login");
  const [email, setEmail] = useState("");
  const [name, setName] = useState("");
  const submit = useAction(async () => {
    if (mode === "login") await loginPasskey(api, email.trim());
    else await registerPasskey(api, email.trim(), name.trim());
    navigate(next, { replace: true });
  });

  if (session !== null && !submit.busy) return <Navigate to={next} replace />;

  return (
    <section className="card narrow">
      <h1>{mode === "login" ? "Sign in" : "Create an account"}</h1>
      <p className="hint">aho cloud uses passkeys: no passwords, and the same passkey signs your release approvals.</p>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          void submit.run();
        }}
      >
        <label>
          Email
          <input type="email" autoComplete="username webauthn" value={email} onChange={(e) => setEmail(e.target.value)} required />
        </label>
        {mode === "register" && (
          <label>
            Name
            <input autoComplete="name" value={name} onChange={(e) => setName(e.target.value)} required />
          </label>
        )}
        <button type="submit" disabled={submit.busy}>
          {submit.busy ? "Waiting for passkey…" : mode === "login" ? "Sign in with passkey" : "Register passkey"}
        </button>
      </form>
      <ErrorBox error={submit.error} />
      <p>
        {mode === "login" ? "New here?" : "Already registered?"}{" "}
        <button
          type="button"
          className="link"
          onClick={() => {
            submit.clear();
            setMode(mode === "login" ? "register" : "login");
          }}
        >
          {mode === "login" ? "Create an account" : "Sign in"}
        </button>
      </p>
    </section>
  );
}
