// `ahocloud login` shows a short code; the signed-in user types it here to bind that device key.
import { useState } from "react";
import { useSearchParams } from "react-router";
import { api } from "../lib/api";
import { ErrorBox } from "../ui/common";
import { useAction } from "../ui/hooks";

export function DevicePage() {
  const [params] = useSearchParams();
  const [code, setCode] = useState(params.get("user_code") ?? params.get("code") ?? "");
  const [approved, setApproved] = useState<string | null>(null);
  const approve = useAction(async () => {
    const user_code = code.trim().toUpperCase();
    await api.approveDevice({ user_code });
    setApproved(user_code);
    setCode("");
  });

  return (
    <section className="card narrow">
      <h1>Approve a device</h1>
      <p className="hint">Enter the code your terminal shows. The device can then sign approvals as you, so only enter codes you started yourself.</p>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          setApproved(null);
          void approve.run();
        }}
      >
        <label>
          Code
          <input className="code" value={code} onChange={(e) => setCode(e.target.value)} placeholder="WDJB-MJHT" autoComplete="off" spellCheck={false} required />
        </label>
        <button type="submit" disabled={approve.busy || code.trim() === ""}>
          Approve device
        </button>
      </form>
      {approved !== null && (
        <p className="ok" role="status">
          Device <code>{approved}</code> approved. Return to your terminal.
        </p>
      )}
      <ErrorBox error={approve.error} />
    </section>
  );
}
