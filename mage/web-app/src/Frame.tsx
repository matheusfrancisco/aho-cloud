// The signed-in frame: `@aho/ui`'s Shell with Mage's brand, the account menu, and a sidebar with the
// org switcher and, inside an org, the Mage pages from `mageNav`.
import { AccountMenu, ChevronDown, Dropdown, IconSquare, LogoMark, OrgIcon, PublicShell, Shell, useAction } from "@aho/ui";
import { atLeast, MageError, mageNav, type SessionMembership } from "@mage/web";
import type { ReactNode } from "react";
import { Link, NavLink, useLocation, useNavigate } from "react-router";
import { rememberOrg, useSession } from "./session";

export function Brand() {
  return (
    <Link to="/" className="brand">
      <LogoMark />
      <span>Mage</span>
    </Link>
  );
}

function Account() {
  const { api, session, loggedOut } = useSession();
  const navigate = useNavigate();
  const logout = useAction(async () => {
    await api.logout();
    loggedOut();
    navigate("/login", { replace: true });
  });
  if (session === undefined || session === null) return null;
  return (
    <>
      <AccountMenu user={session.user}>
        {(close) => (
          <button
            type="button"
            role="menuitem"
            className="menu-item"
            disabled={logout.busy}
            onClick={() => {
              close();
              void logout.run();
            }}
          >
            Log out
          </button>
        )}
      </AccountMenu>
      <MageError error={logout.error} />
    </>
  );
}

function OrgSwitcher({ current }: { current: SessionMembership | null }) {
  const { session } = useSession();
  const navigate = useNavigate();
  const memberships = session?.memberships ?? [];
  return (
    <Dropdown
      buttonClass="org-switcher"
      ariaLabel="Organization"
      label={
        <>
          <OrgIcon />
          <span className="grow">{current?.name ?? "Choose an organization"}</span>
          <ChevronDown />
        </>
      }
    >
      {(close) =>
        memberships.map((m) => (
          <button
            key={m.org}
            type="button"
            role="menuitem"
            className={`menu-item${m.org === current?.org ? " current" : ""}`}
            onClick={() => {
              close();
              if (session) rememberOrg(session.user.id, m.org);
              navigate(`/o/${encodeURIComponent(m.org)}/tickets`);
            }}
          >
            <span className="grow">{m.name}</span>
            <span className="muted small">{m.role}</span>
          </button>
        ))
      }
    </Dropdown>
  );
}

function Nav({ org }: { org: SessionMembership }) {
  const location = useLocation();
  const base = `/o/${encodeURIComponent(org.org)}`;
  return (
    <nav aria-label="Mage pages">
      <div className="side-section">Mage</div>
      {mageNav
        .filter((e) => atLeast(org.role, e.minRole))
        .map((e) => {
          const active = e.activeFor.some((p) => location.pathname.startsWith(`${base}/${p}`));
          return (
            <NavLink key={e.path} to={`${base}/${e.path}`} className={`side-link${active ? " active" : ""}`} aria-current={active ? "page" : undefined}>
              <IconSquare>{e.icon}</IconSquare>
              {e.label}
            </NavLink>
          );
        })}
    </nav>
  );
}

export function Frame({ org, children }: { org: SessionMembership | null; children: ReactNode }) {
  return (
    <Shell
      brand={<Brand />}
      account={<Account />}
      sidebar={
        <>
          <div className="side-label">Organization</div>
          <OrgSwitcher current={org} />
          {org !== null && <Nav org={org} />}
        </>
      }
    >
      {children}
    </Shell>
  );
}

export function PublicFrame({ children }: { children: ReactNode }) {
  return <PublicShell brand={<Brand />}>{children}</PublicShell>;
}
