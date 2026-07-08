import { useInfiniteQuery } from "@tanstack/react-query";

import { client } from "../api/client";
import type { components } from "../api/schema";
import { EntityLink } from "./entity-link";
import { RelTime } from "./rel-time";

type AuditFeedResponse = components["schemas"]["AuditFeedResponse"];

/** The audit-feed routes share one shape; this component serves them all. */
export type AuditEntity = "jobs" | "hosts" | "users" | "image-groups";

const PATHS = {
  jobs: "/jobs/{id}/events",
  hosts: "/hosts/{id}/events",
  users: "/users/{id}/events",
  "image-groups": "/image-groups/{id}/events",
} as const;

function useAuditFeed(entity: AuditEntity, id: string) {
  return useInfiniteQuery({
    queryKey: ["audit", entity, id],
    queryFn: async ({ pageParam }): Promise<AuditFeedResponse> => {
      const { data, response } = await client.GET(PATHS[entity], {
        params: {
          path: { id },
          query: pageParam !== undefined ? { cursor: pageParam } : {},
        },
      });
      if (data === undefined) {
        throw new Error(`fetching audit events failed (${response.status})`);
      }
      return data;
    },
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  });
}

export function AuditLog({ entity, id }: { entity: AuditEntity; id: string }) {
  const feed = useAuditFeed(entity, id);

  return (
    <section>
      <h2>Events</h2>
      {feed.isPending && <p className="muted">Loading…</p>}
      {feed.isError && <p className="error">{feed.error.message}</p>}
      {feed.data && (
        <>
          {feed.data.pages[0]?.events.length === 0 ? (
            <p className="muted">No events.</p>
          ) : (
            <table>
              <thead>
                <tr>
                  <th>When</th>
                  <th>Actor</th>
                  <th>Event</th>
                  <th>Message</th>
                </tr>
              </thead>
              <tbody>
                {feed.data.pages.flatMap((page) =>
                  page.events.map((ev) => (
                    <tr key={ev.event_id}>
                      <td>
                        <RelTime iso={ev.created_at} />
                      </td>
                      <td>
                        <EntityLink kind="user" id={ev.actor_id} />
                      </td>
                      <td className="mono muted">{ev.event_type}</td>
                      <td>{ev.message}</td>
                    </tr>
                  )),
                )}
              </tbody>
            </table>
          )}
          {feed.hasNextPage && (
            <div className="toolbar">
              <button
                disabled={feed.isFetchingNextPage}
                onClick={() => void feed.fetchNextPage()}
              >
                {feed.isFetchingNextPage ? "Loading…" : "Load more"}
              </button>
            </div>
          )}
        </>
      )}
    </section>
  );
}
