// What hosts mount: the Mage pages as routes relative to the host's mount point (e.g. `/o/{org}`
// in mage/web-app), and the sidebar entries for them. Render them inside `<MageProvider>`.
import type { ReactNode } from "react";
import { Navigate, type RouteObject, useRoutes } from "react-router";
import type { MemberRole } from "./gen/mage";
import { OnboardPage } from "./pages/OnboardPage";
import { RepoPage } from "./pages/RepoPage";
import { ReposPage } from "./pages/ReposPage";
import { RunPage } from "./pages/RunPage";
import { SettingsPage } from "./pages/SettingsPage";
import { TicketPage } from "./pages/TicketPage";
import { TicketsPage } from "./pages/TicketsPage";

export const mageRoutes: RouteObject[] = [
  { index: true, element: <Navigate to="tickets" replace /> },
  { path: "tickets", element: <TicketsPage /> },
  { path: "tickets/:ticket", element: <TicketPage /> },
  { path: "runs/:run", element: <RunPage /> },
  { path: "repos", element: <ReposPage /> },
  { path: "repos/:repo", element: <RepoPage /> },
  { path: "repos/:repo/onboard", element: <OnboardPage /> },
  { path: "settings", element: <Navigate to="general" replace /> },
  { path: "settings/:section", element: <SettingsPage /> },
  { path: "*", element: <p className="muted">Not found.</p> },
];

/** The Mage pages under the current route (mount it at `…/*`). */
export function MageRoutes() {
  return useRoutes(mageRoutes);
}

export interface MageNavEntry {
  /** Relative to the mount point. */
  path: string;
  label: string;
  icon: ReactNode;
  /** Lowest role that sees the entry. */
  minRole: MemberRole;
  /** Also active on these sub-paths (e.g. a ticket's runs). */
  activeFor: readonly string[];
}

function Svg({ children }: { children: ReactNode }) {
  return (
    <svg className="icon" width={16} height={16} viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth={1.5} strokeLinecap="round" strokeLinejoin="round" aria-hidden="true" focusable="false">
      {children}
    </svg>
  );
}

export const TicketIcon = () => (
  <Svg>
    <path d="M2.5 4.5a1 1 0 0 1 1-1h9a1 1 0 0 1 1 1v1.75a1.75 1.75 0 0 0 0 3.5v1.75a1 1 0 0 1-1 1h-9a1 1 0 0 1-1-1V9.75a1.75 1.75 0 0 0 0-3.5Z" />
    <path d="M10 3.5v9" strokeDasharray="1.5 1.5" />
  </Svg>
);

export const RepoIcon = () => (
  <Svg>
    <path d="M4 2.5h8.5v9H4.75A1.25 1.25 0 0 0 3.5 12.75V3a.5.5 0 0 1 .5-.5Z" />
    <path d="M3.5 12.75A1.25 1.25 0 0 0 4.75 14h7.75v-2.5" />
    <path d="M6 5h4" />
  </Svg>
);

export const SettingsIcon = () => (
  <Svg>
    <circle cx="8" cy="8" r="2" />
    <path d="M8 1.75v1.5M8 12.75v1.5M1.75 8h1.5M12.75 8h1.5M3.6 3.6l1.05 1.05M11.35 11.35l1.05 1.05M3.6 12.4l1.05-1.05M11.35 4.65l1.05-1.05" />
  </Svg>
);

export const mageNav: readonly MageNavEntry[] = [
  { path: "tickets", label: "Tickets", icon: <TicketIcon />, minRole: "member", activeFor: ["tickets", "runs"] },
  { path: "repos", label: "Repos", icon: <RepoIcon />, minRole: "member", activeFor: ["repos"] },
  { path: "settings/general", label: "Settings", icon: <SettingsIcon />, minRole: "member", activeFor: ["settings"] },
];
