import { Navigate, Outlet, Route, Routes, useLocation } from "react-router";
import { PublicShell, Shell } from "./components/Shell";
import { useSession } from "./lib/session";
import { DevicePage } from "./pages/DevicePage";
import { HomePage } from "./pages/HomePage";
import { LoginPage } from "./pages/LoginPage";
import { OrgPage } from "./pages/OrgPage";
import { PolicyPage } from "./pages/PolicyPage";
import { ProjectPage } from "./pages/ProjectPage";
import { ReleasePage } from "./pages/ReleasePage";
import { TeamPage } from "./pages/TeamPage";

/** Signed-out visitors go to the login page and come back afterwards. */
function RequireAuth() {
  const session = useSession();
  const location = useLocation();
  if (session === null) {
    const next = location.pathname + location.search;
    return <Navigate to={`/login?next=${encodeURIComponent(next)}`} replace />;
  }
  return <Outlet />;
}

export function App() {
  return (
    <Routes>
      <Route element={<PublicShell />}>
        <Route path="/login" element={<LoginPage />} />
      </Route>
      <Route element={<RequireAuth />}>
        <Route element={<Shell />}>
          <Route path="/" element={<HomePage />} />
          <Route path="/device" element={<DevicePage />} />
          <Route path="/orgs/:org" element={<OrgPage />} />
          <Route path="/orgs/:org/team" element={<TeamPage />} />
          <Route path="/orgs/:org/policy" element={<PolicyPage />} />
          <Route path="/projects/:p" element={<ProjectPage />} />
          <Route path="/projects/:p/releases/:r" element={<ReleasePage />} />
          <Route path="*" element={<p className="muted">Not found.</p>} />
        </Route>
      </Route>
    </Routes>
  );
}
