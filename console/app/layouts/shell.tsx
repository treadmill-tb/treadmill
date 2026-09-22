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
        <nav className="container">
          <ul>
            <li>
              <Link to="/" className="brand">
                treadmill
              </Link>
            </li>
          </ul>
          <ul className="sections">
            <li>
              <NavLink to="/jobs">Jobs</NavLink>
            </li>
            <li>
              <NavLink to="/hosts">Hosts</NavLink>
            </li>
            <li>
              <NavLink to="/images">Images</NavLink>
            </li>
            <li>
              <NavLink to="/image-sets">Image sets</NavLink>
            </li>
          </ul>
          <ul>
            <li>
              <Link to="/settings" className="secondary">
                {whoami.data?.name ?? "…"}
              </Link>
            </li>
            <li>
              <button
                onClick={() => {
                  clearToken();
                  void navigate("/login");
                }}
              >
                Log out
              </button>
            </li>
          </ul>
        </nav>
      </header>
      <main className="container">
        <Outlet />
      </main>
    </>
  );
}
