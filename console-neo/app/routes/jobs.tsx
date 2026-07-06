import { useInfiniteQuery } from "@tanstack/react-query";

import { client } from "../api/client";
import type { components } from "../api/schema";
import {
  JobStateBadge,
  TaskExitBadge,
  TerminationBadge,
} from "../components/badges";
import { EntityLink } from "../components/entity-link";
import { ImageRef } from "../components/image-ref";
import { RelTime } from "../components/rel-time";

type JobListResponse = components["schemas"]["JobListResponse"];

export default function Jobs() {
  const jobs = useInfiniteQuery({
    queryKey: ["jobs"],
    queryFn: async ({ pageParam }): Promise<JobListResponse> => {
      const { data, response } = await client.GET("/jobs", {
        params: {
          query: pageParam !== undefined ? { cursor: pageParam } : {},
        },
      });
      if (data === undefined) {
        throw new Error(`fetching jobs failed (${response.status})`);
      }
      return data;
    },
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  });

  return (
    <>
      <h1>Jobs</h1>
      {jobs.isPending && <p className="muted">Loading…</p>}
      {jobs.isError && <p className="error">{jobs.error.message}</p>}
      {jobs.data && (
        <>
          {jobs.data.pages[0]?.jobs.length === 0 ? (
            <p className="muted">No jobs visible to this account.</p>
          ) : (
            <table>
              <thead>
                <tr>
                  <th>Job</th>
                  <th>State</th>
                  <th>Image</th>
                  <th>Host</th>
                  <th>Owner</th>
                  <th>Queued</th>
                  <th>Outcome</th>
                </tr>
              </thead>
              <tbody>
                {jobs.data.pages.flatMap((page) =>
                  page.jobs.map((job) => (
                    <tr key={job.job_id}>
                      <td>
                        <EntityLink kind="job" id={job.job_id} />
                      </td>
                      <td>
                        <JobStateBadge state={job.state} />
                      </td>
                      <td>
                        <ImageRef image={job.image} />
                      </td>
                      <td>
                        <EntityLink
                          kind="host"
                          id={job.dispatched_on_host_id}
                        />
                      </td>
                      <td>
                        <EntityLink kind="user" id={job.owner_id} />
                      </td>
                      <td>
                        <RelTime iso={job.queued_at} />
                      </td>
                      <td>
                        {job.state === "finalized" ? (
                          <>
                            <TaskExitBadge status={job.task_exit_status} />{" "}
                            <TerminationBadge reason={job.termination_reason} />
                          </>
                        ) : (
                          <TaskExitBadge status={job.task_exit_status} />
                        )}
                      </td>
                    </tr>
                  )),
                )}
              </tbody>
            </table>
          )}
          {jobs.hasNextPage && (
            <div className="toolbar">
              <button
                disabled={jobs.isFetchingNextPage}
                onClick={() => void jobs.fetchNextPage()}
              >
                {jobs.isFetchingNextPage ? "Loading…" : "Load more"}
              </button>
            </div>
          )}
        </>
      )}
    </>
  );
}
