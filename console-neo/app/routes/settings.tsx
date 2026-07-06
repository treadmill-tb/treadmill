import { $api } from "../api/client";
import { EntityLink } from "../components/entity-link";
import { RelTime } from "../components/rel-time";

export default function Settings() {
  const me = $api.useQuery("get", "/users/me");
  const tokens = $api.useQuery("get", "/users/me/tokens");

  return (
    <>
      <h1>Settings</h1>
      {me.isPending && <p className="muted">Loading…</p>}
      {me.isError && <p className="error">Failed to load the profile.</p>}
      {me.data && (
        <>
          <section>
            <div className="toolbar">
              <h2>Profile</h2>
              <span className="spacer" />
              {/* TODO(console-neo): wire PATCH /users/me */}
              <button disabled title="Not implemented yet">
                Edit
              </button>
            </div>
            <dl className="props">
              <dt>Username</dt>
              <dd>
                <EntityLink
                  kind="user"
                  id={me.data.user_id}
                  label={me.data.username}
                />
              </dd>
              <dt>Full name</dt>
              <dd>{me.data.full_name ?? <span className="muted">—</span>}</dd>
              <dt>Emails</dt>
              <dd>
                {me.data.emails.map((e) => (
                  <div key={e} className="mono">
                    {e}
                  </div>
                ))}
              </dd>
              <dt>GitHub</dt>
              <dd>
                {me.data.github ? (
                  <a href={me.data.github.profile_url}>
                    {me.data.github.login}
                  </a>
                ) : (
                  <span className="muted">—</span>
                )}
              </dd>
              <dt>Locked</dt>
              <dd>{me.data.locked ? "yes" : "no"}</dd>
            </dl>
          </section>

          <section>
            <h2>Groups</h2>
            {me.data.groups.length === 0 ? (
              <p className="muted">No group memberships.</p>
            ) : (
              <table>
                <thead>
                  <tr>
                    <th>Name</th>
                    <th>Source</th>
                  </tr>
                </thead>
                <tbody>
                  {me.data.groups.map((g) => (
                    <tr key={g.group_id}>
                      <td>{g.name}</td>
                      <td className="muted">
                        {g.source}
                        {g.source_ref !== "" && (
                          <span className="mono"> ({g.source_ref})</span>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </section>
        </>
      )}

      <section>
        <h2>Sessions &amp; API tokens</h2>
        {tokens.isPending && <p className="muted">Loading…</p>}
        {tokens.isError && <p className="error">Failed to load tokens.</p>}
        {tokens.data && (
          <table>
            <thead>
              <tr>
                <th>Token</th>
                <th>Created</th>
                <th>Expires</th>
                <th>Client</th>
                <th>Status</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {tokens.data.map((t) => (
                <tr key={t.token_id}>
                  <td className="mono" title={t.token_id}>
                    {t.token_id.slice(0, 8)}
                    {t.comment != null && (
                      <span className="muted"> {t.comment}</span>
                    )}
                  </td>
                  <td>
                    <RelTime iso={t.created_at} />
                    {t.created_ip != null && (
                      <div className="muted mono">{t.created_ip}</div>
                    )}
                  </td>
                  <td>
                    <RelTime iso={t.expires_at} />
                  </td>
                  <td className="muted">{t.user_agent ?? "—"}</td>
                  <td>
                    {t.current && <span className="badge active">current</span>}{" "}
                    {t.revoked && (
                      <span className="badge danger" title={t.revoked.reason}>
                        revoked
                      </span>
                    )}
                  </td>
                  <td>
                    {/* TODO(console-neo): wire DELETE /users/me/tokens/{token_id} */}
                    <button
                      className="danger"
                      disabled
                      title="Not implemented yet"
                    >
                      Revoke
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
    </>
  );
}
