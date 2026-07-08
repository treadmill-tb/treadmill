import { useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";

import { $api } from "../api/client";
import type { components } from "../api/schema";
import { EntityLink } from "../components/entity-link";
import { MutationError } from "../components/mutation-error";
import { RelTime } from "../components/rel-time";

type SelfUserProfile = components["schemas"]["SelfUserProfile"];

function ProfileForm({
  me,
  onDone,
}: {
  me: SelfUserProfile;
  onDone: () => void;
}) {
  const queryClient = useQueryClient();
  const update = $api.useMutation("patch", "/users/me", {
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["get", "/users/me"] }),
        queryClient.invalidateQueries({ queryKey: ["get", "/auth/whoami"] }),
      ]);
      onDone();
    },
  });

  function onSubmit(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    const f = new FormData(e.currentTarget);
    const str = (k: string): string => {
      const v = f.get(k);
      return typeof v === "string" ? v.trim() : "";
    };

    // PATCH semantics: omitted = unchanged, explicit null = cleared. Username
    // can only change, never clear.
    const body: components["schemas"]["UpdateProfileRequest"] = {};
    const username = str("username");
    if (username !== "" && username !== me.username) {
      body.username = username;
    }
    const fullName = str("full_name");
    if (fullName !== (me.full_name ?? "")) {
      body.full_name = fullName === "" ? null : fullName;
    }
    const avatarUrl = str("avatar_url");
    if (avatarUrl !== (me.avatar_url ?? "")) {
      body.avatar_url = avatarUrl === "" ? null : avatarUrl;
    }
    update.mutate({ body });
  }

  return (
    <form className="form card" onSubmit={onSubmit}>
      <label className="field">
        <span>Username</span>
        <input name="username" defaultValue={me.username} className="mono" />
      </label>
      <label className="field">
        <span>Full name (empty clears)</span>
        <input name="full_name" defaultValue={me.full_name ?? ""} />
      </label>
      <label className="field">
        <span>Avatar URL (empty clears)</span>
        <input
          name="avatar_url"
          defaultValue={me.avatar_url ?? ""}
          className="mono"
        />
      </label>
      <MutationError error={update.error} />
      <div className="toolbar">
        <button type="submit" disabled={update.isPending}>
          {update.isPending ? "Saving…" : "Save"}
        </button>
        <button type="button" onClick={onDone}>
          Cancel
        </button>
      </div>
    </form>
  );
}

export default function Settings() {
  const queryClient = useQueryClient();
  const me = $api.useQuery("get", "/users/me");
  const tokens = $api.useQuery("get", "/users/me/tokens");
  const [editing, setEditing] = useState(false);

  const revoke = $api.useMutation("delete", "/users/me/tokens/{token_id}", {
    onSuccess: async () => {
      await queryClient.invalidateQueries({
        queryKey: ["get", "/users/me/tokens"],
      });
    },
  });

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
              <button onClick={() => setEditing(!editing)}>Edit</button>
            </div>
            {editing ? (
              <ProfileForm me={me.data} onDone={() => setEditing(false)} />
            ) : (
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
            )}
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
        <MutationError error={revoke.error} />
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
                    {t.revoked == null && (
                      <button
                        className="danger"
                        disabled={revoke.isPending}
                        onClick={() => {
                          const q = t.current
                            ? "Revoke the token of THIS session? You will be logged out."
                            : "Revoke this token?";
                          if (window.confirm(q)) {
                            revoke.mutate({
                              params: { path: { token_id: t.token_id } },
                            });
                          }
                        }}
                      >
                        Revoke
                      </button>
                    )}
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
