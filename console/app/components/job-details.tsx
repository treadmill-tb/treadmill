import { Info, Workflow } from "lucide-react";
import { Fragment, useState } from "react";
import { Link } from "react-router";

import { $api } from "../api/client";
import { isStandard } from "../api/images";
import type { components } from "../api/schema";
import { Digest } from "./digest";
import { EntityLink, ShortId, shortId } from "./entity-link";
import { formatSeconds } from "./job-lease";
import { RelTime } from "./rel-time";

type JobInfo = components["schemas"]["JobInfo"];

/** Parameters shown before "Show all". */
const PARAMETERS_SHOWN = 3;

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
        {info.data.label ?? <em>Unnamed Job</em>}
      </Link>{" "}
      <span className="muted">
        (<ShortId id={jobId} />)
      </span>
    </>
  );
}

/**
 * What the job was started with: its image (or the job it continues) and its
 * parameters up front, the rest behind "More details".
 */
export function JobDetails({ job }: { job: JobInfo }) {
  const [allParameters, setAllParameters] = useState(false);
  const [more, setMore] = useState(false);
  const parameters = Object.entries(job.parameters);
  const shown = allParameters
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
          {shown.map(([name, p]) => (
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
