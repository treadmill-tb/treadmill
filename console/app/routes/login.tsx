import { Navigate } from "react-router";

import { $api, API_ORIGIN, getToken } from "../api/client";

/** Provider login URL with this SPA's callback declared as `return_to` (the
 * exact URL must be in the switchboard's `oauth.return_to_allowlist`). */
function providerHref(loginPath: string): string {
  const base = API_ORIGIN === "" ? window.location.origin : API_ORIGIN;
  const url = new URL(loginPath, base);
  url.searchParams.set("return_to", `${window.location.origin}/login/callback`);
  return url.toString();
}

export default function Login() {
  if (getToken() !== null) {
    return <Navigate to="/" replace />;
  }
  return <LoginPage />;
}

function LoginPage() {
  const providers = $api.useQuery("get", "/auth/providers");

  return (
    <main className="page login-page">
      <div className="card login-card">
        <h1>Treadmill</h1>
        {providers.isPending && <p className="muted">Loading login methods…</p>}
        {providers.isError && (
          <p className="error">Failed to load login methods.</p>
        )}
        {providers.data && (
          <>
            {providers.data.oauth.length === 0 &&
              providers.data.mock_identities.length === 0 && (
                <p className="muted">
                  No login methods are configured on this deployment.
                </p>
              )}
            {providers.data.oauth.map((p) => (
              <a
                key={p.name}
                className="btn login-btn"
                href={providerHref(p.login_path)}
              >
                Sign in with {p.display_name}
              </a>
            ))}
            {providers.data.mock_identities.length > 0 && (
              <div className="mock-box">
                <p>
                  <strong>Development only:</strong> unauthenticated mock
                  identities. This must never appear on a production deployment.
                </p>
                {providers.data.mock_identities.map((m) => (
                  <a
                    key={m.key}
                    className="btn login-btn"
                    href={providerHref(m.login_path)}
                  >
                    {m.label}
                  </a>
                ))}
              </div>
            )}
          </>
        )}
      </div>
    </main>
  );
}
