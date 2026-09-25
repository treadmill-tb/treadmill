import { Globe, User, Users } from "lucide-react";
import { useState } from "react";
import { Link } from "react-router";

import { $api } from "../api/client";
import type { components } from "../api/schema";
import { EVERYONE_SUBJECT, isUuid, SYSTEM_SUBJECT } from "../api/subjects";
import { EntityLink, ShortId } from "./entity-link";

type SubjectRef = components["schemas"]["SubjectRef"];

export function SubjectLink({ subject }: { subject: SubjectRef }) {
  const name = subject.name ?? <ShortId id={subject.id} />;
  switch (subject.kind) {
    case "user":
      return (
        <EntityLink
          kind="user"
          id={subject.id}
          label={subject.name ?? undefined}
          icon={User}
        />
      );
    case "group":
      return (
        <span className="subject" title={subject.id}>
          <Users size={14} aria-hidden="true" /> {name}
        </span>
      );
    case "job":
      return <EntityLink kind="job" id={subject.id} />;
    default:
      if (subject.id === EVERYONE_SUBJECT)
        return (
          <span className="subject">
            <Globe size={14} aria-hidden="true" /> Everyone
          </span>
        );
      return (
        <span className="subject" title={subject.id}>
          {subject.id === SYSTEM_SUBJECT ? "Treadmill" : name}
        </span>
      );
  }
}

/** A group the viewer isn't in has no name to look up: short ID. */
export function SubjectName({ id }: { id: string }) {
  const me = $api.useQuery("get", "/users/me");
  const special = id === EVERYONE_SUBJECT || id === SYSTEM_SUBJECT;
  const group = me.data?.groups.find((g) => g.group_id === id);
  const user = $api.useQuery(
    "get",
    "/users/{id}",
    { params: { path: { id } } },
    {
      enabled: !special && me.data !== undefined && group === undefined,
      retry: false,
    },
  );

  if (id === EVERYONE_SUBJECT) {
    return (
      <span className="subject">
        <Globe size={14} aria-hidden="true" /> Everyone
      </span>
    );
  }
  if (id === SYSTEM_SUBJECT) return <span className="subject">Treadmill</span>;
  if (group !== undefined) {
    return (
      <span className="subject" title={id}>
        <Users size={14} aria-hidden="true" /> {group.name}
      </span>
    );
  }
  if (user.data !== undefined) {
    return (
      <Link to={`/users/${id}`} className="subject" title={id}>
        <User size={14} aria-hidden="true" /> {user.data.name}
        {me.data?.user_id === id && <span className="muted"> (you)</span>}
      </Link>
    );
  }
  return (
    <span className="subject">
      <Users size={14} aria-hidden="true" /> <ShortId id={id} />
    </span>
  );
}

const BY_ID = "by-id";

/** One of the viewer's groups, or a user by ID (until subject search). */
export function SubjectPicker({
  onChange,
}: {
  onChange: (subject: string | null) => void;
}) {
  const me = $api.useQuery("get", "/users/me");
  const [choice, setChoice] = useState("");
  const [userId, setUserId] = useState("");
  const trimmed = userId.trim();
  const user = $api.useQuery(
    "get",
    "/users/{id}",
    { params: { path: { id: trimmed } } },
    { enabled: choice === BY_ID && isUuid(trimmed), retry: false },
  );

  function choose(value: string) {
    setChoice(value);
    onChange(value === "" || value === BY_ID ? null : value);
  }

  return (
    <>
      <select
        aria-label="Group or user"
        value={choice}
        onChange={(e) => choose(e.target.value)}
      >
        <option value="">Group or user…</option>
        {me.data?.groups
          .filter((g) => g.group_id !== EVERYONE_SUBJECT)
          .map((g) => (
            <option key={g.group_id} value={g.group_id}>
              Group: {g.name}
            </option>
          ))}
        <option value={BY_ID}>User by ID…</option>
      </select>
      {choice === BY_ID && (
        <div className="field">
          <input
            aria-label="User ID"
            className="mono"
            placeholder="User ID"
            value={userId}
            onChange={(e) => {
              setUserId(e.target.value);
              const id = e.target.value.trim();
              onChange(isUuid(id) ? id : null);
            }}
          />
          <small className="muted">
            {!isUuid(trimmed)
              ? trimmed === ""
                ? null
                : "Invalid ID"
              : user.isPending
                ? "…"
                : user.data !== undefined
                  ? `${user.data.name}${user.data.github ? ` (@${user.data.github.login})` : ""}`
                  : "Unknown user"}
          </small>
        </div>
      )}
    </>
  );
}
