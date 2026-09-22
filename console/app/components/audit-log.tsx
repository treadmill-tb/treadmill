import { useInfiniteQuery } from "@tanstack/react-query";
import { useState } from "react";

import { client } from "../api/client";
import { ApiError } from "../api/errors";
import type { components } from "../api/schema";
import { EntityLink } from "./entity-link";
import { MutationError } from "./mutation-error";
import { RelTime } from "./rel-time";

type AuditFeedResponse = components["schemas"]["AuditFeedResponse"];

/** The audit-feed routes share one shape; this component serves them all. */
export type AuditEntity = "jobs" | "hosts" | "users" | "image-sets";

const PATHS = {
  jobs: "/jobs/{id}/events",
  hosts: "/hosts/{id}/events",
  users: "/users/{id}/events",
  "image-sets": "/image-sets/{id}/events",
} as const;

function useAuditFeed(entity: AuditEntity, id: string, enabled: boolean) {
  return useInfiniteQuery({
    enabled,
    queryKey: ["audit", entity, id],
    queryFn: async ({ pageParam }): Promise<AuditFeedResponse> => {
      const { data, error, response } = await client.GET(PATHS[entity], {
        params: {
          path: { id },
          query: pageParam !== undefined ? { cursor: pageParam } : {},
        },
      });
      if (data === undefined) {
        throw new ApiError(response.status, error);
      }
      return data;
    },
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  });
}

/** An entity's audit events, collapsed until the reader opens them — and only
 * fetched then, since most visits never do. */
export function AuditLog({ entity, id }: { entity: AuditEntity; id: string }) {
  const [open, setOpen] = useState(false);
  const feed = useAuditFeed(entity, id, open);

  return (
    <section>
      <details
        className="collapsible"
        onToggle={(e) => setOpen(e.currentTarget.open)}
      >
        <summary>
          <h2>Events</h2>
        </summary>
        {open && <AuditEvents feed={feed} />}
      </details>
    </section>
  );
}

function AuditEvents({ feed }: { feed: ReturnType<typeof useAuditFeed> }) {
  return (
    <>
      {feed.isPending && <p className="muted">Loading…</p>}
      <MutationError
        error={feed.error}
        messages={{ 403: "You are not allowed to see these events." }}
      />
      {feed.data && (
        <>
          {feed.data.pages[0]?.events.length === 0 ? (
            <p className="muted">No events.</p>
          ) : (
            <div className="overflow-auto">
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
            </div>
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
    </>
  );
}
