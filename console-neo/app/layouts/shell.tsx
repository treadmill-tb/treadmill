import { Link, Navigate, NavLink, Outlet, useNavigate } from "react-router";

import { $api, clearToken, getToken } from "../api/client";

export default function Shell() {
  if (getToken() === null) {
    return <Navigate to="/login" replace />;
  }
  return <AuthedShell />;
}

function AuthedShell() {
  const navigate = useNavigate();
  const whoami = $api.useQuery("get", "/auth/whoami");

  return (
    <>
      <header className="topbar">
        <div className="topbar-inner">
          <Link to="/" className="brand">
            treadmill
          </Link>
          <nav>
            <NavLink to="/jobs">Jobs</NavLink>
            <NavLink to="/hosts">Hosts</NavLink>
            <NavLink to="/images">Images</NavLink>
            <NavLink to="/image-groups">Image groups</NavLink>
          </nav>
          <span className="spacer" />
          <Link to="/settings" className="muted">
            {whoami.data?.username ?? "…"}
          </Link>
          <button
            onClick={() => {
              clearToken();
              void navigate("/login");
            }}
          >
            Log out
          </button>
        </div>
      </header>
      <main className="page">
        <Outlet />
      </main>
    </>
  );
}
