import { useState, type ReactNode } from "react";

import { $api } from "../api/client";
import type { components } from "../api/schema";
import { EVERYONE_SUBJECT, isUuid, SYSTEM_SUBJECT } from "../api/subjects";
import { Dialog } from "./dialog";
import { RequestError } from "./request-error";

type SelfUserProfile = components["schemas"]["SelfUserProfile"];

type Props = {
  current: string | null;
  system?: ReactNode;
  pending: boolean;
  error: unknown;
  errorMessages: Record<number, string>;
  onSave: (owner: string | null) => void;
  onClose: () => void;
};

function Choice({
  checked,
  onPick,
  children,
}: {
  checked: boolean;
  onPick: () => void;
  children: ReactNode;
}) {
  return (
    <label>
      <input type="radio" name="owner" checked={checked} onChange={onPick} />
      <span>
        <strong>{children}</strong>
      </span>
    </label>
  );
}

export function OwnerDialog(props: Props) {
  const me = $api.useQuery("get", "/users/me");
  if (me.data === undefined) {
    return (
      <Dialog open onClose={props.onClose} title="Owner">
        {me.isPending && <p className="muted">Loading…</p>}
        <RequestError error={me.error} />
      </Dialog>
    );
  }
  return <OwnerChoices {...props} me={me.data} />;
}

function OwnerChoices({
  current,
  system,
  pending,
  error,
  errorMessages,
  onSave,
  onClose,
  me,
}: Props & { me: SelfUserProfile }) {
  const myId = me.user_id;
  const groups = me.groups.filter((g) => g.group_id !== EVERYONE_SUBJECT);
  const known = [
    myId,
    system != null ? SYSTEM_SUBJECT : undefined,
    ...groups.map((g) => g.group_id),
  ];
  const [choice, setChoice] = useState<string | null>(current);
  const [byId, setById] = useState(
    current !== null && !known.includes(current),
  );
  const [userId, setUserId] = useState(byId ? (current ?? "") : "");

  const owner = byId ? userId.trim() : choice;
  const valid = owner === null || isUuid(owner);
  const leaving = owner !== myId && !groups.some((g) => g.group_id === owner);

  function pick(value: string | null) {
    setById(false);
    setChoice(value);
  }

  return (
    <Dialog
      open
      onClose={onClose}
      title="Owner"
      footer={
        <>
          <button type="button" onClick={onClose}>
            Cancel
          </button>
          <button
            type="button"
            className="primary"
            disabled={pending || !valid || owner === current}
            onClick={() => onSave(owner)}
          >
            {pending ? "Saving…" : "Save"}
          </button>
        </>
      }
    >
      <fieldset className="choice-list">
        {system != null && (
          <Choice
            checked={!byId && choice === SYSTEM_SUBJECT}
            onPick={() => pick(SYSTEM_SUBJECT)}
          >
            {system}
          </Choice>
        )}
        <Choice checked={!byId && choice === myId} onPick={() => pick(myId)}>
          {me.name} (you)
        </Choice>
        {groups.map((g) => (
          <Choice
            key={g.group_id}
            checked={!byId && choice === g.group_id}
            onPick={() => pick(g.group_id)}
          >
            Group: {g.name}
          </Choice>
        ))}
        <Choice checked={byId} onPick={() => setById(true)}>
          User by ID
        </Choice>
        {byId && (
          <input
            aria-label="User ID"
            className="mono"
            placeholder="User ID"
            value={userId}
            onChange={(e) => setUserId(e.target.value)}
          />
        )}
        <Choice checked={!byId && choice === null} onPick={() => pick(null)}>
          No owner (global admins only)
        </Choice>
      </fieldset>
      {leaving && owner !== current && valid && (
        <p className="muted">You may lose access.</p>
      )}
      <RequestError error={error} messages={errorMessages} />
    </Dialog>
  );
}
