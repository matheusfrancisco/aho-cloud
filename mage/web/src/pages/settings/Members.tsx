// Settings → Members: members and pending invites (Member reads); invite by GitHub login with a
// role up to the caller's (Admin), withdraw invites and remove Members (Admin), change roles and
// remove anyone (Owner). The server enforces the same rules (last Owner, role ceiling).
import { Ago, Loading, useAction, useLoad } from "@aho/ui";
import { useState } from "react";
import type { OrgApi } from "../../api/client";
import { MageError } from "../../components/common";
import { useFeed, useMage } from "../../context";
import type { MemberRole, MemberView } from "../../gen/mage";
import { atLeast, ROLE_LABEL, ROLES } from "../../lib/labels";

function RoleSelect({ value, max, onChange, label }: { value: MemberRole; max: MemberRole; onChange: (r: MemberRole) => void; label: string }) {
  return (
    <select aria-label={label} value={value} onChange={(e) => onChange(ROLES.find((r) => r === e.target.value) ?? value)}>
      {ROLES.filter((r) => atLeast(max, r)).map((r) => (
        <option key={r} value={r}>
          {ROLE_LABEL[r]}
        </option>
      ))}
    </select>
  );
}

function MemberRow({ api, member, onChanged }: { api: OrgApi; member: MemberView; onChanged: () => void }) {
  const { can, host } = useMage();
  const self = member.user.id === host.user.id;
  const setRole = useAction(async (role: MemberRole) => {
    await api.setRole(member.user.id, { role });
    onChanged();
  });
  const remove = useAction(async () => {
    await api.removeMember(member.user.id);
    onChanged();
  });
  const canRemove = !self && (can("owner") || (can("admin") && member.role === "member"));
  return (
    <tr>
      <td>
        {member.user.name}
        {self && <span className="muted small"> (you)</span>}
        <div className="muted small">{member.user.email}</div>
      </td>
      <td>{can("owner") && !self ? <RoleSelect label={`Role of ${member.user.name}`} value={member.role} max="owner" onChange={(r) => void setRole.run(r)} /> : ROLE_LABEL[member.role]}</td>
      <td>
        {canRemove && (
          <button type="button" className="secondary small" onClick={() => void remove.run()} disabled={remove.busy}>
            Remove
          </button>
        )}
        <MageError error={setRole.error ?? remove.error} />
      </td>
    </tr>
  );
}

function InviteForm({ api, onInvited }: { api: OrgApi; onInvited: () => void }) {
  const { role } = useMage();
  const [login, setLogin] = useState("");
  const [invitedRole, setInvitedRole] = useState<MemberRole>("member");
  const invite = useAction(async () => {
    await api.invite({ github_login: login.trim(), role: invitedRole });
    setLogin("");
    onInvited();
  });
  return (
    <form
      className="row"
      aria-label="Invite"
      onSubmit={(e) => {
        e.preventDefault();
        if (login.trim() !== "") void invite.run();
      }}
    >
      <input className="grow" aria-label="GitHub login" placeholder="GitHub login" value={login} onChange={(e) => setLogin(e.target.value)} autoComplete="off" spellCheck={false} />
      <RoleSelect label="Invited role" value={invitedRole} max={role} onChange={setInvitedRole} />
      <button type="submit" disabled={invite.busy || login.trim() === ""}>
        {invite.busy ? "Inviting…" : "Invite"}
      </button>
      <MageError error={invite.error} />
    </form>
  );
}

export function MembersSettings() {
  const { api, can } = useMage();
  const loaded = useLoad(() => api.members(), [api]);
  useFeed((e) => e.kind === "member", loaded.reload);
  const withdraw = useAction(async (invite: string) => {
    await api.withdrawInvite(invite);
    loaded.reload();
  });
  if (loaded.data === undefined) return loaded.loading ? <Loading what="members" /> : <MageError error={loaded.error} />;
  const { members, invites } = loaded.data;
  return (
    <div className="stack">
      <table aria-label="Members">
        <thead>
          <tr>
            <th>Member</th>
            <th>Role</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {members.map((m) => (
            <MemberRow key={m.user.id} api={api} member={m} onChanged={loaded.reload} />
          ))}
        </tbody>
      </table>
      <h3>Pending invites</h3>
      {invites.length === 0 ? (
        <p className="muted">No pending invites.</p>
      ) : (
        <table aria-label="Invites">
          <thead>
            <tr>
              <th>GitHub login</th>
              <th>Role</th>
              <th>Expires</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {invites.map((i) => (
              <tr key={i.id}>
                <td>{i.login}</td>
                <td>{ROLE_LABEL[i.role]}</td>
                <td>
                  <Ago ms={Date.parse(i.expires_at)} />
                </td>
                <td>
                  {can("admin") && (
                    <button type="button" className="secondary small" onClick={() => void withdraw.run(i.id)} disabled={withdraw.busy}>
                      Withdraw
                    </button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <MageError error={withdraw.error} />
      {can("admin") && (
        <>
          <h3>Invite</h3>
          <p className="hint">The invite turns into a membership the next time that GitHub user signs in to Mage (it expires after 14 days).</p>
          <InviteForm api={api} onInvited={loaded.reload} />
        </>
      )}
    </div>
  );
}
