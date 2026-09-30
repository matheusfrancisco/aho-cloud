// The signed-in frame: top bar with the account menu, and a sidebar with the org switcher, the
// Create menu, and the org's pages. Pages read the shell through `useShell`.
import { createContext, useContext, useState } from "react";
import { Link, NavLink, Outlet, useLocation, useNavigate } from "react-router";
import type { Me } from "../gen/api";
import { api } from "../lib/api";
import { setCurrentOrg, useCurrentOrg } from "../lib/remembered";
import { signOut, useSession } from "../lib/session";
import { slugProblem } from "../lib/slug";
import { ErrorBox } from "../ui/common";
import { useAction, useLoad } from "../ui/hooks";
import { ChevronDown, IconSquare, LogoMark, OrgIcon, PlusIcon, PolicyIcon, ProjectsIcon, RocketIcon, TeamIcon, UserIcon } from "../ui/icons";
import { Dropdown } from "../ui/overlay";
import { CreateProjectModal } from "./CreateProject";

export interface ShellState {
  me: Me | undefined;
  meError: unknown;
  reloadMe: () => void;
  /** The org the sidebar is scoped to. */
  org: string | null;
  createProject: () => void;
}

export const ShellContext = createContext<ShellState>({ me: undefined, meError: null, reloadMe: () => {}, org: null, createProject: () => {} });

export function useShell(): ShellState {
  return useContext(ShellContext);
}

function Brand() {
  return (
    <Link to="/" className="brand">
      <LogoMark />
      <span>aho cloud</span>
    </Link>
  );
}

function AccountMenu() {
  const session = useSession();
  const navigate = useNavigate();
  if (session === null) return null;
  return (
    <Dropdown
      buttonClass="topbar-button"
      align="right"
      label={
        <>
          <UserIcon /> Account <ChevronDown />
        </>
      }
    >
      {(close) => (
        <>
          <div className="menu-head">
            <strong>{session.user.name}</strong>
            <span className="muted">{session.user.email}</span>
          </div>
          <Link role="menuitem" className="menu-item" to="/device" onClick={close}>
            Approve a device
          </Link>
          <button
            type="button"
            role="menuitem"
            className="menu-item"
            onClick={() => {
              close();
              signOut();
              navigate("/login");
            }}
          >
            Log out
          </button>
        </>
      )}
    </Dropdown>
  );
}

export function NewOrgForm({ onCreated }: { onCreated: (slug: string) => void }) {
  const [slug, setSlug] = useState("");
  const [name, setName] = useState("");
  const problem = slugProblem(slug);
  const create = useAction(async () => {
    const org = await api.createOrg({ slug, name: name.trim() });
    onCreated(org.slug);
  });
  return (
    <form
      className="stack"
      onSubmit={(e) => {
        e.preventDefault();
        if (problem === null) void create.run();
      }}
    >
      <label>
        Slug
        <input value={slug} onChange={(e) => setSlug(e.target.value.toLowerCase())} placeholder="acme" autoComplete="off" spellCheck={false} />
      </label>
      {slug !== "" && problem !== null && <p className="field-error">{problem}</p>}
      <label>
        Name
        <input value={name} onChange={(e) => setName(e.target.value)} placeholder="Acme Inc." />
      </label>
      <button type="submit" disabled={create.busy || problem !== null || name.trim() === ""}>
        {create.busy ? "Creating…" : "Create organization"}
      </button>
      <ErrorBox error={create.error} />
    </form>
  );
}

/** The user's orgs and an inline form for a new one. Used by the sidebar and the first-run page. */
export function OrgSwitcher() {
  const { me, org, reloadMe } = useShell();
  const navigate = useNavigate();
  const [creating, setCreating] = useState(false);
  const go = (slug: string) => {
    setCurrentOrg(slug);
    navigate(`/orgs/${encodeURIComponent(slug)}`);
  };
  return (
    <Dropdown
      buttonClass="org-switcher"
      ariaLabel="Organization"
      label={
        <>
          <OrgIcon />
          <span className="grow">{org ?? "Choose an organization"}</span>
          <ChevronDown />
        </>
      }
    >
      {(close) => (
        <>
          {me?.orgs.map((m) => (
            <button
              key={m.org}
              type="button"
              role="menuitem"
              className={`menu-item${m.org === org ? " current" : ""}`}
              onClick={() => {
                close();
                go(m.org);
              }}
            >
              <span className="grow">{m.org}</span>
              {m.owner && <span className="muted small">owner</span>}
            </button>
          ))}
          {me !== undefined && me.orgs.length > 0 && <hr />}
          {creating ? (
            <div className="menu-form">
              <NewOrgForm
                onCreated={(slug) => {
                  close();
                  setCreating(false);
                  reloadMe();
                  go(slug);
                }}
              />
            </div>
          ) : (
            <button type="button" className="menu-item" onClick={() => setCreating(true)}>
              <PlusIcon /> New organization
            </button>
          )}
        </>
      )}
    </Dropdown>
  );
}

function CreateMenu() {
  const { org, createProject } = useShell();
  return (
    <Dropdown
      buttonClass="create-button"
      label={
        <>
          <PlusIcon /> Create <ChevronDown />
        </>
      }
    >
      {(close) => (
        <button
          type="button"
          role="menuitem"
          className="menu-item rich"
          disabled={org === null}
          onClick={() => {
            close();
            createProject();
          }}
        >
          <IconSquare>
            <RocketIcon />
          </IconSquare>
          <span>
            <strong>New project</strong>
            <span className="muted small">Chat with aho and deploy</span>
          </span>
        </button>
      )}
    </Dropdown>
  );
}

function Sidebar() {
  const { org } = useShell();
  const location = useLocation();
  const base = org === null ? null : `/orgs/${encodeURIComponent(org)}`;
  const inProject = location.pathname.startsWith("/projects/");
  return (
    <aside className="sidebar">
      <div className="side-label">Organization</div>
      <OrgSwitcher />
      <CreateMenu />
      {base !== null && (
        <nav aria-label="Organization pages">
          <div className="side-section">Build</div>
          <NavLink to={base} end className={({ isActive }) => `side-link${isActive || inProject ? " active" : ""}`}>
            <IconSquare>
              <ProjectsIcon />
            </IconSquare>
            Projects
          </NavLink>
          <div className="side-section">Organization</div>
          <NavLink to={`${base}/team`} className={({ isActive }) => `side-link${isActive ? " active" : ""}`}>
            <IconSquare>
              <TeamIcon />
            </IconSquare>
            Team
          </NavLink>
          <NavLink to={`${base}/policy`} className={({ isActive }) => `side-link${isActive ? " active" : ""}`}>
            <IconSquare>
              <PolicyIcon />
            </IconSquare>
            Release policy
          </NavLink>
        </nav>
      )}
    </aside>
  );
}

export function Shell() {
  const me = useLoad(() => api.me(), []);
  const remembered = useCurrentOrg();
  const [creating, setCreating] = useState(false);
  const org = remembered ?? me.data?.orgs[0]?.org ?? null;
  const state: ShellState = { me: me.data, meError: me.error, reloadMe: me.reload, org, createProject: () => setCreating(true) };
  return (
    <ShellContext.Provider value={state}>
      <header className="topbar">
        <Brand />
        <div className="topbar-right">
          <AccountMenu />
        </div>
      </header>
      <div className="layout">
        <Sidebar />
        <main className="content">
          <Outlet />
        </main>
      </div>
      {creating && org !== null && <CreateProjectModal org={org} onClose={() => setCreating(false)} />}
    </ShellContext.Provider>
  );
}

/** Signed-out frame: just the brand. */
export function PublicShell() {
  return (
    <>
      <header className="topbar">
        <Brand />
      </header>
      <main className="public">
        <Outlet />
      </main>
    </>
  );
}
