import { $api } from "../api/client";
import { jobImageName } from "../api/images";
import type { components } from "../api/schema";
import { shortId } from "./entity-link";

type JobInfo = components["schemas"]["JobInfo"];

export function JobName({
  label,
  imageName,
  hostId,
  hostName,
  finished,
}: {
  label: string | null | undefined;
  imageName: string;
  hostId: string | null | undefined;
  hostName: string | null | undefined;
  finished: boolean;
}) {
  if (label != null) {
    return <q className="job-name">{label}</q>;
  }
  const host = hostId == null ? null : (hostName ?? shortId(hostId));
  return (
    <em className="job-name">
      {imageName}
      {host !== null ? (
        <>
          <span className="job-name-sep"> on </span>
          {host}
        </>
      ) : (
        !finished && <span className="job-name-sep"> · to be scheduled</span>
      )}
    </em>
  );
}

export function JobInfoName({ job }: { job: JobInfo }) {
  const ref = job.image.reference;
  const set = $api.useQuery(
    "get",
    "/image-sets/{id}",
    { params: { path: { id: ref.type === "image_set" ? ref.set_id : "" } } },
    { enabled: ref.type === "image_set" && job.label == null },
  );
  const hosts = $api.useQuery("get", "/hosts", undefined, {
    enabled: job.dispatched_on_host_id != null && job.label == null,
  });
  return (
    <JobName
      label={job.label}
      imageName={jobImageName(ref, set.data?.display_name)}
      hostId={job.dispatched_on_host_id}
      hostName={
        hosts.data?.find((h) => h.host_id === job.dispatched_on_host_id)?.name
      }
      finished={job.state === "finalized"}
    />
  );
}
