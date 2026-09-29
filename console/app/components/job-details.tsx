import { Info, Workflow, Pencil, Plus, Check, Trash2, X } from "lucide-react";
import { Fragment, useState } from "react";
import { Link } from "react-router";

import { $api } from "../api/client";
import { isStandard } from "../api/images";
import type { components } from "../api/schema";
import { Digest } from "./digest";
import { EntityLink, ShortId, shortId } from "./entity-link";
import { formatSeconds } from "./job-lease";
import { JobInfoName } from "./job-name";
import { RelTime } from "./rel-time";
import { RequestError } from "./request-error";
import { useUpdateJob } from "../hooks/use-update-job";

type JobInfo = components["schemas"]["JobInfo"];

/** Parameters shown before "Show all". */
const PARAMETERS_SHOWN = 3;

/** Annotations shown before "Show all". */
const ANNOTATIONS_SHOWN = 3;

/** What the job runs, as the first rows of the details card. */
function Origin({ job }: { job: JobInfo }) {
  const { reference: ref, resolved_digest } = job.image;
  return (
    <>
      {job.predecessor != null && (
        <>
          <dt>{job.predecessor.type === "resume" ? "Resumes" : "Restarts"}</dt>
          <dd>
            <JobOrigin jobId={job.predecessor.job_id} />
          </dd>
        </>
      )}
      <dt>Image</dt>
      <dd>
        {ref.type === "image" ? (
          <ImageOrigin digest={ref.manifest_digest} />
        ) : (
          <ImageSetOrigin setId={ref.set_id} generation={ref.generation} />
        )}
      </dd>
      {ref.type === "image_set" && resolved_digest != null && (
        <>
          <dt>Build</dt>
          <dd>
            <Digest digest={resolved_digest} />{" "}
            <Link to={`/images/build/${resolved_digest}`}>view</Link>
          </dd>
        </>
      )}
    </>
  );
}

/** An image by its title, if it has one, then its digest. */
function ImageOrigin({ digest }: { digest: string }) {
  const info = $api.useQuery("get", "/images/{digest}", {
    params: { path: { digest } },
  });
  const title = info.data?.title;
  if (title == null) {
    return (
      <>
        <Digest digest={digest} />{" "}
        <Link to={`/images/build/${digest}`}>view</Link>
      </>
    );
  }
  return (
    <>
      <Link
        to={`/images/build/${digest}`}
        className="origin-name"
        title={title}
      >
        {title}
      </Link>{" "}
      <span className="muted">
        (<Digest digest={digest} />)
      </span>
    </>
  );
}

function ImageSetOrigin({
  setId,
  generation,
}: {
  setId: string;
  generation: number;
}) {
  const info = $api.useQuery("get", "/image-sets/{id}", {
    params: { path: { id: setId } },
  });
  const to = `/images/${setId}/versions/${generation}`;
  if (info.data === undefined) {
    return (
      <Link to={to} className="short-id" title={setId}>
        {shortId(setId)} v{generation}
      </Link>
    );
  }
  const name = info.data.display_name;
  const latest = info.data.latest_generation;
  return (
    <>
      <Link to={to} className="origin-name" title={name}>
        {name}
      </Link>{" "}
      v{generation}
      {isStandard(info.data) && (
        <>
          {" "}
          <span className="badge ok">Standard</span>
        </>
      )}
      {latest != null && latest !== generation && (
        <span className="muted">
          {" "}
          (<Link to={`/images/${setId}`}>v{latest}</Link> is the latest)
        </span>
      )}
    </>
  );
}

/** The job this one continues, by name, then its short ID. */
function JobOrigin({ jobId }: { jobId: string }) {
  const info = $api.useQuery("get", "/jobs/{id}", {
    params: { path: { id: jobId } },
  });
  if (info.data === undefined) {
    return <EntityLink kind="job" id={jobId} icon={Workflow} />;
  }
  return (
    <>
      <Link to={`/jobs/${jobId}`} className="origin-name">
        <JobInfoName job={info.data} />
      </Link>{" "}
      <span className="muted">
        (<ShortId id={jobId} />)
      </span>
    </>
  );
}

export function JobAnnotations({ job }: { job: JobInfo }) {
  const [allAnnotations, setAllAnnotations] = useState(false);
  const annotations = Object.entries(job.annotations);
  const shownAnnotations = allAnnotations
    ? annotations
    : annotations.slice(0, ANNOTATIONS_SHOWN);
  const update = useUpdateJob(job.job_id);
  const [draft, setDraft] = useState<
    { key: number; name: string; value: string }[] | null
  >(null);
  const cancel = () => {
    setDraft(null);
    update.reset();
  };

  if (draft !== null) {
    const updateAnnotation = (key: number, name: string, value: string) => {
      setDraft((d) =>
        (d ?? []).map((old) => {
          if (key !== old.key) {
            return old;
          } else {
            return {
              key,
              name,
              value,
            };
          }
        }),
      );
    };

    return (
      <>
        <form
          className="set-annotations"
          onSubmit={(e) => {
            e.preventDefault();

            const draftNames = new Set(draft.map(({ name }) => name));
            const deletions = Object.fromEntries(
              Object.keys(job.annotations)
                .filter((k) => !draftNames.has(k))
                .map((k) => [k, null]),
            );
            const updates = Object.fromEntries(
              draft.flatMap(({ name, value }) => {
                if (
                  Object.hasOwn(job.annotations, name) &&
                  job.annotations[name] === value
                ) {
                  return []; // Nothing to update
                } else {
                  return [[name, value]];
                }
              }),
            );
            const patch = { ...deletions, ...updates };

            if (Object.keys(patch).length === 0) {
              setDraft(null);
            } else {
              update.mutate(
                {
                  params: { path: { id: job.job_id } },
                  body: { annotations: patch },
                },
                { onSuccess: () => setDraft(null) },
              );
            }
          }}
        >
          <div className="job-annotations-header">
            <h4>Annotations</h4>
            <button
              type="submit"
              className="icon-btn"
              title="Save job annotations"
              aria-label="Save job annotations"
              disabled={update.isPending}
            >
              <Check size={20} aria-hidden="true" />
            </button>
            <button
              type="button"
              className="icon-btn"
              title="Cancel"
              aria-label="Cancel editing annotations"
              onClick={cancel}
            >
              <X size={20} aria-hidden="true" />
            </button>
            <button
              type="button"
              className="icon-btn job-annotations-add"
              title="Add a new job annotation field"
              aria-label="Add a new job annotation field"
              onClick={() => {
                const newKey = (draft.at(-1)?.key ?? 0) + 1;
                setDraft((d) =>
                  (d ?? []).concat([{ key: newKey, name: "", value: "" }]),
                );
              }}
            >
              <Plus size={18} aria-hidden="true" />
            </button>
          </div>
          {draft.length === 0 ? (
            <p className="muted">No annotations.</p>
          ) : (
            <dl className="props truncate">
              {draft.map(({ key, name, value }) => (
                <Fragment key={key}>
                  <dt className="mono" title={name}>
                    <input
                      value={name}
                      aria-label="The name / key of this job annotation"
                      onChange={(e) =>
                        updateAnnotation(key, e.target.value, value)
                      }
                    />
                  </dt>
                  <dd className="mono job-annotation-value">
                    <input
                      value={value}
                      aria-label="The value of this job annotation"
                      onChange={(e) =>
                        updateAnnotation(key, name, e.target.value)
                      }
                    />
                    <button
                      type="button"
                      className="icon-btn"
                      title="Delete annotation"
                      aria-label={
                        name === ""
                          ? "Delete annotation"
                          : `Delete annotation ${name}`
                      }
                      onClick={() =>
                        setDraft((d) => (d ?? []).filter((a) => a.key !== key))
                      }
                    >
                      <Trash2 size={18} aria-hidden="true" />
                    </button>
                  </dd>
                </Fragment>
              ))}
            </dl>
          )}
          <RequestError
            error={update.error}
            messages={{
              403: "You are not allowed to change this job's annotations.",
            }}
          />
        </form>
      </>
    );
  }

  return (
    <>
      <div className="job-annotations-header">
        <h4>Annotations</h4>
        {job.permissions.includes("manage") && (
          <button
            type="button"
            className="icon-btn"
            title="Modify job annotations"
            aria-label="Modify job annotations"
            onClick={() =>
              setDraft(
                annotations.map(([name, value], idx) => ({
                  key: idx,
                  name,
                  value,
                })),
              )
            }
          >
            <Pencil size={18} aria-hidden="true" />
          </button>
        )}
      </div>
      {annotations.length === 0 ? (
        <p className="muted">No annotations.</p>
      ) : (
        <dl className="props truncate">
          {shownAnnotations.map(([name, value]) => (
            <Fragment key={name}>
              <dt className="mono" title={name}>
                {name}
              </dt>
              <dd className="mono" title={value}>
                {value}
              </dd>
            </Fragment>
          ))}
        </dl>
      )}
      {annotations.length > ANNOTATIONS_SHOWN && (
        <button
          type="button"
          className="link-btn"
          onClick={() => setAllAnnotations((a) => !a)}
        >
          {allAnnotations
            ? "Show fewer annotations"
            : `Show all ${annotations.length} annotations`}
        </button>
      )}
    </>
  );
}

export function JobDetails({ job }: { job: JobInfo }) {
  const [allParameters, setAllParameters] = useState(false);
  const [more, setMore] = useState(false);
  const parameters = Object.entries(job.parameters);
  const shownParameters = allParameters
    ? parameters
    : parameters.slice(0, PARAMETERS_SHOWN);

  return (
    <section className="card job-details">
      <h3 className="card-head">
        <Info size={18} aria-hidden="true" />
        Job Details
      </h3>
      <dl className="props">
        <Origin job={job} />
      </dl>

      <h4>Parameters</h4>
      {parameters.length === 0 ? (
        <p className="muted">No parameters.</p>
      ) : (
        <dl className="props truncate">
          {shownParameters.map(([name, p]) => (
            <Fragment key={name}>
              <dt className="mono" title={name}>
                {name}
              </dt>
              <dd title={p.secret ? undefined : (p.value ?? undefined)}>
                {p.secret ? (
                  <span className="badge warn" title="Value withheld">
                    secret
                  </span>
                ) : (
                  <span className="mono">{p.value}</span>
                )}
              </dd>
            </Fragment>
          ))}
        </dl>
      )}
      {parameters.length > PARAMETERS_SHOWN && (
        <button
          type="button"
          className="link-btn"
          onClick={() => setAllParameters((a) => !a)}
        >
          {allParameters
            ? "Show fewer parameters"
            : `Show all ${parameters.length} parameters`}
        </button>
      )}

      <JobAnnotations job={job} />

      {more && (
        <dl className="props more-props">
          {job.job_ip_address != null && (
            <>
              <dt>Address</dt>
              <dd className="mono">{job.job_ip_address}</dd>
            </>
          )}
          <dt>Queued</dt>
          <dd>
            <RelTime iso={job.queued_at} />
          </dd>
          <dt>Started</dt>
          <dd>
            <RelTime iso={job.started_at} />
          </dd>
          <dt>Ended</dt>
          <dd>
            <RelTime iso={job.terminated_at} />
          </dd>
          <dt>Lease length</dt>
          <dd>{formatSeconds(job.lease_duration_secs)}</dd>
          <dt>Restarts left</dt>
          <dd>{job.restart_policy.remaining_restarts}</dd>
          <dt>Host predicate</dt>
          <dd>
            <code>{job.host_cel_predicate}</code>
          </dd>
        </dl>
      )}
      <p className="details-more">
        <button
          type="button"
          className="link-btn"
          aria-expanded={more}
          onClick={() => setMore((m) => !m)}
        >
          {more ? "Fewer details" : "More details"}
        </button>
      </p>
    </section>
  );
}
