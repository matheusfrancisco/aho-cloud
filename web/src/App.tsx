import { Link, Navigate, Outlet, Route, Routes, useLocation, useNavigate } from "react-router";
import { signOut, useSession } from "./lib/session";
import { DevicePage } from "./pages/DevicePage";
import { LoginPage } from "./pages/LoginPage";
import { OrgPage } from "./pages/OrgPage";
import { OrgsPage } from "./pages/OrgsPage";
import { ProjectPage } from "./pages/ProjectPage";
import { ReleasePage } from "./pages/ReleasePage";
import { WorkspacePage } from "./pages/WorkspacePage";

function Shell() {
  const session = useSession();
  const navigate = useNavigate();
  return (
    <>
      <header className="topbar">
        <Link to="/" className="brand">
          aho cloud
        </Link>
        <nav>
          <Link to="/device">Approve a device</Link>
        </nav>
        {session !== null && (
          <div className="me">
            <span>{session.user.email}</span>
            <button
              type="button"
              className="secondary"
              onClick={() => {
                signOut();
                navigate("/login");
              }}
            >
              Log out
            </button>
          </div>
        )}
      </header>
      <main>
        <Outlet />
      </main>
    </>
  );
}

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
      <Route element={<Shell />}>
        <Route path="/login" element={<LoginPage />} />
        <Route element={<RequireAuth />}>
          <Route path="/" element={<OrgsPage />} />
          <Route path="/device" element={<DevicePage />} />
          <Route path="/orgs/:org" element={<OrgPage />} />
          <Route path="/projects/:p" element={<ProjectPage />} />
          <Route path="/projects/:p/releases/:r" element={<ReleasePage />} />
          <Route path="/projects/:p/workspace" element={<WorkspacePage />} />
        </Route>
        <Route path="*" element={<p>Not found.</p>} />
      </Route>
    </Routes>
  );
}
