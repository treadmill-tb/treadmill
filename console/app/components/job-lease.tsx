import { useEffect, useState } from "react";

import { ApiError, type ErrorMessages } from "../api/errors";
import type { components } from "../api/schema";
import { useNow } from "../hooks/use-now";
import { useUpdateJob } from "../hooks/use-update-job";
import { Dialog } from "./dialog";
import { MutationError } from "./mutation-error";

type JobInfo = components["schemas"]["JobInfo"];
type JobLeaseExpiryAction = components["schemas"]["JobLeaseExpiryAction"];
type LeaseRejection = components["schemas"]["LeaseRejection"];

/** Below this much remaining lease, the countdown turns to a warning. */
const LOW_LEASE_MS = 10 * 60 * 1000;

/** One-click lease extensions. */
const EXTENSIONS = [
  { label: "+30 min", ms: 30 * 60 * 1000 },
  { label: "+1 h", ms: 60 * 60 * 1000 },
  { label: "+4 h", ms: 4 * 60 * 60 * 1000 },
];

/** How long a newly set end time stays highlighted. */
const FLASH_MS = 1500;

/** A duration in seconds as `2h 30m`, down to seconds only when that's all. */
export function formatSeconds(secs: number): string {
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  return [h && `${h}h`, m && `${m}m`, (s || !(h || m)) && `${s}s`]
    .filter(Boolean)
    .join(" ");
}

/** A countdown, only as precise as its size warrants. */
function formatRemaining(ms: number): string {
  const secs = Math.max(0, Math.floor(ms / 1000));
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  if (secs >= LOW_LEASE_MS / 1000) return `${m}m`;
  return `${m}m ${String(s).padStart(2, "0")}s`;
}

/** A wall-clock time, with the date only when it isn't today. */
function formatClock(ms: number): string {
  const date = new Date(ms);
  const today = new Date().toDateString() === date.toDateString();
  return date.toLocaleString(undefined, {
    ...(today ? {} : { month: "short", day: "numeric" }),
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** The `YYYY-MM-DDTHH:mm` a `datetime-local` input takes, in local time. */
function toLocalInput(ms: number): string {
  const date = new Date(ms - new Date(ms).getTimezoneOffset() * 60_000);
  return date.toISOString().slice(0, 16);
}

function isLeaseRejection(body: unknown): body is LeaseRejection {
  return (
    typeof body === "object" &&
    body !== null &&
    "code" in body &&
    "message" in body
  );
}

/** `PATCH /jobs/{id}` failures other than a lease refusal. */
const UPDATE_ERRORS: ErrorMessages = {
  403: "You are not allowed to change this job's lease.",
};

const EXPIRY_ACTIONS: Record<
  JobLeaseExpiryAction,
  { sentence: string; label: string; detail: string }
> = {
  terminate: {
    sentence: "terminate the job",
    label: "Terminate the job",
    detail: "The job stops as soon as its lease runs out.",
  },
  preempt: {
    sentence: "allow the host to be reclaimed",
    label: "Allow the host to be reclaimed",
    detail:
      "The job keeps running past its lease, until another job needs the host.",
  },
};

/**
 * A job's lease: how long it has left, ways to extend or shorten it, and what
 * happens when it runs out.
 *
 * Before the job starts there is no end time yet, so only the length and the
 * expiry action are shown (the switchboard refuses lease changes until then).
 */
export function JobLease({
  job,
  canManage,
}: {
  job: JobInfo;
  canManage: boolean;
}) {
  const update = useUpdateJob(job.job_id);
  const now = useNow();
  const [dialog, setDialog] = useState<"end" | "action" | null>(null);
  const [flash, setFlash] = useState(false);
  useEffect(() => {
    if (!flash) return;
    const timer = setTimeout(() => setFlash(false), FLASH_MS);
    return () => clearTimeout(timer);
  }, [flash]);

  const setLease = (lease: string) => {
    update.mutate(
      { params: { path: { id: job.job_id } }, body: { lease } },
      {
        onSuccess: () => {
          setDialog(null);
          setFlash(true);
        },
      },
    );
  };
  const setAction = (action: JobLeaseExpiryAction) => {
    update.mutate(
      {
        params: { path: { id: job.job_id } },
        body: { lease_expiry_action: action },
      },
      { onSuccess: () => setDialog(null) },
    );
  };

  const expires =
    job.lease_expires_at != null ? Date.parse(job.lease_expires_at) : null;
  const started = job.started_at != null ? Date.parse(job.started_at) : null;
  const canChange = canManage && job.state !== "terminating";

  const onLeaseEnd = (
    <p className="lease-action">
      On lease end:{" "}
      <strong>{EXPIRY_ACTIONS[job.lease_expiry_action].sentence}</strong>.
      {canChange && (
        <button
          type="button"
          className="link-btn"
          onClick={() => {
            update.reset();
            setDialog("action");
          }}
        >
          Change
        </button>
      )}
    </p>
  );
  // Dialogs mount only while open, so each starts from the job as it is then.
  const actionDialog = dialog === "action" && (
    <ExpiryActionDialog
      current={job.lease_expiry_action}
      pending={update.isPending}
      error={update.error}
      onSave={setAction}
      onClose={() => setDialog(null)}
    />
  );

  if (expires === null || started === null) {
    return (
      <div className="lease">
        <p>
          A <strong>{formatSeconds(job.lease_duration_secs)}</strong> lease,
          counted from when the job starts.
        </p>
        {onLeaseEnd}
        {actionDialog}
      </div>
    );
  }

  const remaining = expires - now;
  const low = remaining < LOW_LEASE_MS;
  // Extending an expired lease counts from now, not from the end in the past.
  const extend = (ms: number) =>
    remaining > 0
      ? setLease(`+${Math.round(ms / 60_000)}m`)
      : setLease(new Date(now + ms).toISOString());

  return (
    <div className="lease">
      <p className={`lease-headline${flash ? " flash" : ""}`}>
        {remaining > 0 ? (
          <>
            Lease ends in{" "}
            <strong className={low ? "lease-low" : undefined}>
              {formatRemaining(remaining)}
            </strong>{" "}
            <span className="muted">(at {formatClock(expires)})</span>
          </>
        ) : (
          <>
            Lease ended <span className="muted">at {formatClock(expires)}</span>
          </>
        )}
      </p>
      <progress
        className={low ? "warn" : undefined}
        value={Math.min(now - started, expires - started)}
        max={Math.max(expires - started, 1)}
      />
      {canChange && (
        <div className="lease-buttons">
          {EXTENSIONS.map(({ label, ms }) => (
            <button
              key={label}
              type="button"
              disabled={update.isPending}
              onClick={() => extend(ms)}
            >
              {label}
            </button>
          ))}
          <button
            type="button"
            disabled={update.isPending}
            onClick={() => {
              update.reset();
              setDialog("end");
            }}
          >
            Set end time…
          </button>
        </div>
      )}
      {dialog === null && (
        <LeaseError
          error={update.error}
          expires={expires}
          onAccept={(iso) => setLease(iso)}
        />
      )}
      {onLeaseEnd}
      {actionDialog}
      {dialog === "end" && (
        <EndTimeDialog
          expires={expires}
          now={now}
          pending={update.isPending}
          error={update.error}
          onSave={(ms) => setLease(new Date(ms).toISOString())}
          onClose={() => setDialog(null)}
        />
      )}
    </div>
  );
}

/** A refused lease change, phrased as what can be done instead. */
function LeaseError({
  error,
  expires,
  onAccept,
}: {
  error: unknown;
  expires: number;
  onAccept?: (iso: string) => void;
}) {
  const rejection =
    error instanceof ApiError && error.status === 409 ? error.body : undefined;
  if (!isLeaseRejection(rejection)) {
    return <MutationError error={error} messages={UPDATE_ERRORS} />;
  }
  let text: string;
  let offer: string | null = null;
  switch (rejection.code) {
    case "policy_limit": {
      const max =
        rejection.max_lease_expires_at != null
          ? Date.parse(rejection.max_lease_expires_at)
          : null;
      if (max === null) {
        text = "The lease can't be extended any further.";
      } else {
        text = `The latest the lease can end is ${formatClock(max)}.`;
        if (max > expires) offer = rejection.max_lease_expires_at ?? null;
      }
      break;
    }
    case "resource_pressure":
      text =
        "Hosts are in demand right now, so the lease can't be extended." +
        (rejection.retry_after_secs != null
          ? ` Try again in ${formatRemaining(rejection.retry_after_secs * 1000)}.`
          : "");
      break;
    case "job_terminating":
      text = "The job is already shutting down.";
      break;
    case "not_started":
      text = "The lease can only be changed once the job has started.";
      break;
    default:
      text = rejection.message;
  }
  return (
    <p className="error">
      {text}
      {offer !== null && onAccept !== undefined && (
        <button
          type="button"
          className="link-btn"
          onClick={() => onAccept(offer)}
        >
          Extend to {formatClock(Date.parse(offer))}
        </button>
      )}
    </p>
  );
}

function EndTimeDialog({
  expires,
  now,
  pending,
  error,
  onSave,
  onClose,
}: {
  expires: number;
  now: number;
  pending: boolean;
  error: unknown;
  onSave: (ms: number) => void;
  onClose: () => void;
}) {
  const [value, setValue] = useState(() =>
    toLocalInput(Math.max(expires, Date.now())),
  );
  const chosen = Date.parse(value);
  const valid = !Number.isNaN(chosen) && chosen > now;

  return (
    <Dialog
      open
      onClose={onClose}
      title="Set the lease's end"
      footer={
        <>
          <button type="button" onClick={onClose}>
            Cancel
          </button>
          <button
            type="submit"
            form="lease-end-form"
            disabled={!valid || pending}
          >
            {pending ? "Saving…" : "Set end time"}
          </button>
        </>
      }
    >
      <form
        id="lease-end-form"
        onSubmit={(e) => {
          e.preventDefault();
          if (valid) onSave(chosen);
        }}
      >
        <label>
          End the lease at
          <input
            type="datetime-local"
            value={value}
            min={toLocalInput(now)}
            onChange={(e) => setValue(e.target.value)}
          />
        </label>
        <p className="muted">
          {Number.isNaN(chosen)
            ? "Pick a date and time."
            : chosen <= now
              ? "That's in the past."
              : `That's ${formatRemaining(chosen - now)} from now` +
                (chosen < expires
                  ? `, ${formatRemaining(expires - chosen)} earlier than now.`
                  : ".")}
        </p>
        <LeaseError error={error} expires={expires} />
      </form>
    </Dialog>
  );
}

function ExpiryActionDialog({
  current,
  pending,
  error,
  onSave,
  onClose,
}: {
  current: JobLeaseExpiryAction;
  pending: boolean;
  error: unknown;
  onSave: (action: JobLeaseExpiryAction) => void;
  onClose: () => void;
}) {
  const [choice, setChoice] = useState(current);

  return (
    <Dialog
      open
      onClose={onClose}
      title="When the lease ends"
      footer={
        <>
          <button type="button" onClick={onClose}>
            Cancel
          </button>
          <button
            type="button"
            className="primary"
            disabled={choice === current || pending}
            onClick={() => onSave(choice)}
          >
            {pending ? "Saving…" : "Save"}
          </button>
        </>
      }
    >
      <fieldset className="choice-list">
        {(Object.keys(EXPIRY_ACTIONS) as JobLeaseExpiryAction[]).map((a) => (
          <label key={a}>
            <input
              type="radio"
              name="lease-expiry-action"
              checked={choice === a}
              onChange={() => setChoice(a)}
            />
            <span>
              <strong>{EXPIRY_ACTIONS[a].label}</strong>
              <small className="muted">{EXPIRY_ACTIONS[a].detail}</small>
            </span>
          </label>
        ))}
      </fieldset>
      <MutationError error={error} messages={UPDATE_ERRORS} />
    </Dialog>
  );
}
