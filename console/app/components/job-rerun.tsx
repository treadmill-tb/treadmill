import { Link } from "react-router";

import type { components } from "../api/schema";
import { HelpTip } from "./help-tip";

type JobInfo = components["schemas"]["JobInfo"];

export function RerunButtons({ job }: { job: JobInfo }) {
  return (
    <>
      {job.dispatched_on_host_id != null && (
        <Link className="btn" to={`/jobs/new?resume=${job.job_id}`}>
          Resume
        </Link>
      )}
      {job.predecessor?.type !== "resume" && (
        <Link className="btn" to={`/jobs/new?restart=${job.job_id}`}>
          Restart
        </Link>
      )}
      <HelpTip label="Resume or restart">
        <strong>Resume</strong> starts a new job on the same host, continuing
        from this job&rsquo;s disk (if it still exists).{" "}
        <strong>Restart</strong> starts a new job from the same image version,
        on any matching host. A resumed job can&rsquo;t be restarted.
      </HelpTip>
    </>
  );
}
