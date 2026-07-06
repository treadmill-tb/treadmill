import { Link, Navigate, Outlet, useNavigate } from "react-router";

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
          <span className="spacer" />
          <span className="muted">{whoami.data?.username}</span>
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
