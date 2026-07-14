import { fetchEventSource } from "@microsoft/fetch-event-source";
import { type QueryKey, useQueryClient } from "@tanstack/react-query";
import { useEffect } from "react";

import { API_ORIGIN, clearToken, getToken } from "../api/client";

/** A response that must not be retried (the token or grant is gone). */
class FatalError extends Error {}

/**
 * Subscribe to a resource's `/watch` SSE channel and invalidate `invalidateKey`
 * on every change, so the corresponding react-query refetches. The connection
 * carries the bearer token, reconnects across the server's channel TTL, and is
 * torn down when the component unmounts. A `401` clears the session and returns
 * to login; a `403` (grant revoked) stops watching without retrying.
 */
export function useResourceWatch(
  watchPath: string,
  invalidateKey: QueryKey,
): void {
  const queryClient = useQueryClient();
  const serializedKey = JSON.stringify(invalidateKey);

  useEffect(() => {
    const controller = new AbortController();
    const queryKey = JSON.parse(serializedKey) as QueryKey;

    void fetchEventSource(`${API_ORIGIN}/api/v1${watchPath}`, {
      signal: controller.signal,
      headers: { Authorization: `Bearer ${getToken() ?? ""}` },
      onopen(response) {
        if (response.ok) {
          return Promise.resolve();
        }
        if (response.status === 401 && getToken() !== null) {
          clearToken();
          window.location.assign("/login");
        }
        throw new FatalError(`watch ${watchPath} failed: ${response.status}`);
      },
      onmessage(event) {
        if (event.event === "change") {
          void queryClient.invalidateQueries({ queryKey });
        }
      },
      onerror(err) {
        // A fatal open aborts the whole thing; transient errors fall through to
        // the library's automatic reconnect.
        if (err instanceof FatalError) {
          throw err;
        }
      },
    }).catch(() => {
      // Unmount abort or a fatal error: nothing left to do here.
    });

    return () => controller.abort();
  }, [watchPath, serializedKey, queryClient]);
}
