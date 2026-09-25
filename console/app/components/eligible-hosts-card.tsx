import { Server } from "lucide-react";

import { $api } from "../api/client";
import { jobImageSpec } from "../api/hosts";
import type { components } from "../api/schema";
import { HostMatchSummary, matchVerdict, splitCandidates } from "./job-preview";
import { RequestError } from "./request-error";

type HostListEntry = components["schemas"]["HostListEntry"];
type JobInfo = components["schemas"]["JobInfo"];

export function EligibleHostsCard({
  job,
  hosts,
}: {
  job: JobInfo;
  hosts: HostListEntry[] | undefined;
}) {
  const me = $api.useQuery("get", "/users/me");
  const ownerId = job.owner?.id;
  const ownerView =
    ownerId != null &&
    (ownerId === me.data?.user_id ||
      (me.data?.groups.some((g) => g.group_id === ownerId) ?? false));
  const report = $api.useQuery(
    "post",
    "/hosts/match",
    {
      body: {
        host_cel_predicate: job.host_cel_predicate,
        init_spec:
          job.predecessor?.type === "resume" ? null : jobImageSpec(job),
        owner: ownerId ?? null,
      },
    },
    { enabled: ownerView },
  );
  const { eligible, incompatible } = splitCandidates(report.data, hosts);

  return (
    <aside className="card host-card">
      <h3 className="card-head">
        <Server size={18} aria-hidden="true" />
        Eligible Hosts
      </h3>
      {me.data !== undefined && !ownerView && (
        <p className="muted">Visible to the job's owner</p>
      )}
      {ownerView && report.isPending && <p className="muted">Finding hosts…</p>}
      <RequestError
        error={report.error}
        messages={{
          403: "You may not use this job's image, so its eligible hosts can't be determined.",
        }}
      />
      {report.data !== undefined && (
        <HostMatchSummary
          verdict={matchVerdict(report.data, eligible)}
          hostsTitle={`Eligible hosts · ${eligible.length}`}
          hosts={[...eligible, ...incompatible]}
          report={report.data}
          showFunnel={job.host_cel_predicate.trim() !== "true"}
        />
      )}
    </aside>
  );
}
