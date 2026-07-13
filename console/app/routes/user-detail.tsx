import { $api } from "../api/client";
import { AuditLog } from "../components/audit-log";
import type { Route } from "./+types/user-detail";

export default function UserDetail({ params }: Route.ComponentProps) {
  const user = $api.useQuery("get", "/users/{id}", {
    params: { path: { id: params.id } },
  });

  return (
    <>
      {user.isPending && <p className="muted">Loading…</p>}
      {user.isError && <p className="error">Failed to load the user.</p>}
      {user.data && (
        <>
          <h1>
            {user.data.avatar_url != null && (
              <img className="avatar" src={user.data.avatar_url} alt="" />
            )}{" "}
            {user.data.name}
          </h1>
          <dl className="props">
            <dt>Id</dt>
            <dd className="mono">{user.data.user_id}</dd>
            <dt>GitHub</dt>
            <dd>
              {user.data.github ? (
                <a href={user.data.github.profile_url}>
                  {user.data.github.login}
                </a>
              ) : (
                <span className="muted">—</span>
              )}
            </dd>
          </dl>

          <AuditLog entity="users" id={params.id} />
        </>
      )}
    </>
  );
}
