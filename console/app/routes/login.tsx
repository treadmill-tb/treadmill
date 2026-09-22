import { useState } from "react";
import { Navigate, useNavigate } from "react-router";

import { $api, API_ORIGIN, getToken } from "../api/client";

const RETURN_TO = `${window.location.origin}/login/callback`;

function providerHref(loginPath: string, redirect: boolean): string {
  const base = API_ORIGIN === "" ? window.location.origin : API_ORIGIN;
  const url = new URL(loginPath, base);
  if (redirect) {
    url.searchParams.set("return_to", RETURN_TO);
  }
  return url.toString();
}

export default function Login() {
  if (getToken() !== null) {
    return <Navigate to="/" replace />;
  }
  return <LoginPage />;
}

function LoginPage() {
  const navigate = useNavigate();
  const [code, setCode] = useState("");
  const providers = $api.useQuery("get", "/auth/providers", {
    params: { query: { return_to: RETURN_TO } },
  });
  const redirect = providers.data?.return_to_allowed ?? false;
  const target = redirect ? undefined : "_blank";

  return (
    <main className="container login-page">
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
                href={providerHref(p.login_path, redirect)}
                target={target}
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
                    href={providerHref(m.login_path, redirect)}
                    target={target}
                  >
                    {m.label}
                  </a>
                ))}
              </div>
            )}
            {!redirect && (
              <form
                className="form"
                onSubmit={(e) => {
                  e.preventDefault();
                  void navigate(
                    `/login/callback?login_code=${encodeURIComponent(code.trim())}`,
                  );
                }}
              >
                <label className="field">
                  <span>Login code</span>
                  <input
                    className="mono"
                    required
                    autoComplete="off"
                    value={code}
                    onChange={(e) => setCode(e.target.value)}
                  />
                </label>
                <button type="submit">Sign in</button>
              </form>
            )}
          </>
        )}
      </div>
    </main>
  );
}
