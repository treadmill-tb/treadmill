import { Globe, Lock, TriangleAlert } from "lucide-react";
import { useState } from "react";

import { EVERYONE_SUBJECT } from "../api/subjects";
import { Dialog } from "./dialog";
import { HelpTip } from "./help-tip";
import { RequestError } from "./request-error";
import { SubjectName, SubjectPicker } from "./subject";

/** A named set of permissions; roles are cumulative. */
export type Role<P extends string> = {
  label: string;
  detail?: string;
  permissions: P[];
};

export type ShareGrant<P extends string> = {
  subject_id: string;
  permission: P;
  revocable?: boolean;
};

/** `blocked` changed nothing; applying with `force` goes ahead anyway. */
export type ShareResult = { blocked: string[] } | null;

/** Give `subject` exactly `permissions` on the resource. */
export type ApplyAccess<P extends string> = (
  subject: string,
  permissions: P[],
  force: boolean,
) => Promise<ShareResult>;

function sameSet<P>(a: readonly P[], b: readonly P[]): boolean {
  return a.length === b.length && a.every((p) => b.includes(p));
}

function roleOf<P extends string>(
  roles: Role<P>[],
  permissions: P[],
): Role<P> | undefined {
  return roles.find((r) => sameSet(r.permissions, permissions));
}

export function ShareDialog<P extends string>({
  open,
  onClose,
  title,
  ownerId,
  ownerRole = "Owner",
  grants,
  grantsError,
  roles,
  publicLevels,
  apply,
}: {
  open: boolean;
  onClose: () => void;
  title: string;
  ownerId: string | null | undefined;
  ownerRole?: string;
  grants: ShareGrant<P>[] | undefined;
  grantsError: unknown;
  roles: Role<P>[];
  publicLevels: Role<P>[];
  apply: ApplyAccess<P>;
}) {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [blocked, setBlocked] = useState<{
    names: string[];
    retry: () => void;
  } | null>(null);
  const [newSubject, setNewSubject] = useState<string | null>(null);
  const [newRole, setNewRole] = useState(0);
  const [pickerKey, setPickerKey] = useState(0);

  const bySubject = new Map<string, ShareGrant<P>[]>();
  for (const g of grants ?? []) {
    bySubject.set(g.subject_id, [...(bySubject.get(g.subject_id) ?? []), g]);
  }
  const everyone = (bySubject.get(EVERYONE_SUBJECT) ?? []).map(
    (g) => g.permission,
  );
  const people = [...bySubject.entries()].filter(
    ([id]) => id !== EVERYONE_SUBJECT && id !== ownerId,
  );

  async function run(subject: string, permissions: P[], force = false) {
    setPending(true);
    setError(null);
    setBlocked(null);
    try {
      const result = await apply(subject, permissions, force);
      if (result !== null && result.blocked.length > 0) {
        setBlocked({
          names: result.blocked,
          retry: () => void run(subject, permissions, true),
        });
        return false;
      }
      return true;
    } catch (e) {
      setError(e);
      return false;
    } finally {
      setPending(false);
    }
  }

  const publicLevel = publicLevels.findIndex((l) =>
    sameSet(l.permissions, everyone),
  );

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={`Share “${title}”`}
      footer={
        <button type="button" onClick={onClose}>
          Done
        </button>
      }
    >
      <RequestError
        error={grantsError}
        messages={{ 403: "Only managers can see who has access." }}
      />
      {grants !== undefined && (
        <>
          <h4>General access</h4>
          <fieldset className="choice-list">
            <label>
              <input
                type="radio"
                name="general-access"
                disabled={pending}
                checked={everyone.length === 0}
                onChange={() => void run(EVERYONE_SUBJECT, [])}
              />
              <span>
                <strong>
                  <Lock size={14} aria-hidden="true" /> Private
                </strong>
              </span>
            </label>
            {publicLevels.map((level, i) => (
              <label key={level.label}>
                <input
                  type="radio"
                  name="general-access"
                  disabled={pending}
                  checked={publicLevel === i}
                  onChange={() => void run(EVERYONE_SUBJECT, level.permissions)}
                />
                <span>
                  <strong>
                    <Globe size={14} aria-hidden="true" /> {level.label}
                  </strong>
                </span>
              </label>
            ))}
            {everyone.length > 0 && publicLevel < 0 && (
              <p className="muted">
                Everyone currently has: {everyone.join(", ")}.
              </p>
            )}
          </fieldset>

          <h4>
            People and groups{" "}
            <HelpTip label="About roles">
              {roles.map((r) => (
                <span key={r.label} className="help-line">
                  <strong>{r.label}:</strong> {r.detail}
                </span>
              ))}
            </HelpTip>
          </h4>
          <ul className="share-list">
            {ownerId != null && (
              <li>
                <SubjectName id={ownerId} />
                <span className="muted">{ownerRole}</span>
              </li>
            )}
            {people.map(([subject, held]) => {
              const permissions = held.map((g) => g.permission);
              const fixed = held.some((g) => g.revocable === false);
              const role = roleOf(roles, permissions);
              return (
                <li key={subject}>
                  <SubjectName id={subject} />
                  {fixed ? (
                    <span
                      className="muted"
                      title="Set by the switchboard; it can't be removed."
                    >
                      <Lock size={14} aria-hidden="true" />{" "}
                      {role?.label ?? permissions.join(", ")}
                    </span>
                  ) : (
                    <span className="share-role">
                      <select
                        aria-label="Role"
                        disabled={pending}
                        value={role === undefined ? "" : roles.indexOf(role)}
                        onChange={(e) => {
                          const next = roles[Number(e.target.value)];
                          if (next !== undefined) {
                            void run(subject, next.permissions);
                          }
                        }}
                      >
                        {role === undefined && (
                          <option value="">{permissions.join(", ")}</option>
                        )}
                        {roles.map((r, i) => (
                          <option key={r.label} value={i}>
                            {r.label}
                          </option>
                        ))}
                      </select>
                      <button
                        type="button"
                        className="danger"
                        disabled={pending}
                        onClick={() => void run(subject, [])}
                      >
                        Remove
                      </button>
                    </span>
                  )}
                </li>
              );
            })}
          </ul>

          <div className="share-add">
            <SubjectPicker key={pickerKey} onChange={setNewSubject} />
            <select
              aria-label="Role for the new subject"
              value={newRole}
              onChange={(e) => setNewRole(Number(e.target.value))}
            >
              {roles.map((r, i) => (
                <option key={r.label} value={i}>
                  {r.label}
                </option>
              ))}
            </select>
            <button
              type="button"
              className="primary"
              disabled={pending || newSubject === null}
              onClick={() => {
                const role = roles[newRole];
                if (newSubject === null || role === undefined) return;
                void run(newSubject, role.permissions).then((ok) => {
                  if (ok) {
                    setNewSubject(null);
                    setPickerKey((k) => k + 1);
                  }
                });
              }}
            >
              Add
            </button>
          </div>
        </>
      )}

      {blocked !== null && (
        <div className="notice">
          <TriangleAlert size={18} aria-hidden="true" />
          <div>
            <p>
              Can&rsquo;t share: <strong>{blocked.names.join(", ")}</strong>{" "}
              <HelpTip label="Why">
                You don&rsquo;t manage where these builds are stored. Unless
                their owners have shared them, jobs won&rsquo;t run on these
                platforms for the people you share with.
              </HelpTip>
            </p>
            <button
              type="button"
              disabled={pending}
              onClick={() => setBlocked(null)}
            >
              Cancel
            </button>{" "}
            <button
              type="button"
              className="danger"
              disabled={pending}
              onClick={blocked.retry}
            >
              Share anyway
            </button>
          </div>
        </div>
      )}
      <RequestError error={error} />
    </Dialog>
  );
}
