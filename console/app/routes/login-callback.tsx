import { useEffect, useRef, useState } from "react";
import { Link, useNavigate, useSearchParams } from "react-router";

import { $api, client, setToken } from "../api/client";
import type { components } from "../api/schema";

type Staged = components["schemas"]["LoginStagedResponse"];

type Phase =
  | { kind: "completing" }
  | { kind: "tos"; staged: Staged }
  | { kind: "error"; message: string };

export default function LoginCallback() {
  const [params] = useSearchParams();
  const navigate = useNavigate();
  const stagedId = params.get("staged_id");
  const stagedSecret = params.get("staged_secret");
  const [phase, setPhase] = useState<Phase>(() =>
    stagedId === null || stagedSecret === null
      ? {
          kind: "error",
          message: "Missing staged login credentials in the callback URL.",
        }
      : { kind: "completing" },
  );
  const started = useRef(false);

  const tos = $api.useQuery("get", "/auth/tos", undefined, {
    enabled: phase.kind === "tos",
  });

  async function complete(
    stagedId: string,
    stagedSecret: string,
    tosVersion: number | null,
  ): Promise<void> {
    const { data, error, response } = await client.POST(
      "/auth/login/complete",
      {
        body: {
          staged_id: stagedId,
          staged_secret: stagedSecret,
          tos_version: tosVersion,
        },
      },
    );
    if (data) {
      setToken(data.token);
      await navigate("/", { replace: true });
      return;
    }
    if (response.status === 409 && error) {
      // The presented pair was consumed; the response carries a fresh one.
      // Today the only known step is the ToS interstitial.
      const staged = error;
      if (staged.required.includes("tos")) {
        setPhase({ kind: "tos", staged });
        void tos.refetch();
        return;
      }
      setPhase({
        kind: "error",
        message: `The login requires unsupported steps: ${staged.required.join(", ")}.`,
      });
      return;
    }
    if (response.status === 410) {
      setPhase({
        kind: "error",
        message: "This login attempt expired or was already used.",
      });
      return;
    }
    setPhase({
      kind: "error",
      message: `Completing the login failed (HTTP ${response.status}).`,
    });
  }

  useEffect(() => {
    if (started.current || stagedId === null || stagedSecret === null) return;
    started.current = true;
    void complete(stagedId, stagedSecret, null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <main className="page login-page">
      <div className="card login-card">
        <h1>Signing in…</h1>
        {phase.kind === "completing" && (
          <p className="muted">Completing the login.</p>
        )}
        {phase.kind === "tos" && (
          <>
            <h2>Terms of Service</h2>
            {tos.isPending && <p className="muted">Loading…</p>}
            {tos.isError && (
              <p className="error">Failed to load the Terms of Service.</p>
            )}
            {tos.data && (
              <>
                <pre className="tos-text">{tos.data.text}</pre>
                <button
                  onClick={() => {
                    // Echo the version of the text actually shown, so consent
                    // is recorded against it.
                    void complete(
                      phase.staged.staged_id,
                      phase.staged.staged_secret,
                      tos.data.version,
                    );
                  }}
                >
                  Accept and continue
                </button>
              </>
            )}
          </>
        )}
        {phase.kind === "error" && (
          <>
            <p className="error">{phase.message}</p>
            <Link to="/login">Back to login</Link>
          </>
        )}
      </div>
    </main>
  );
}
