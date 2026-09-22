import { useQueryClient } from "@tanstack/react-query";

import { $api } from "../api/client";

/** `PATCH /jobs/{id}`, refreshing everything that shows the job afterwards. */
export function useUpdateJob(jobId: string) {
  const queryClient = useQueryClient();
  return $api.useMutation("patch", "/jobs/{id}", {
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["get", "/jobs/{id}"] }),
        queryClient.invalidateQueries({ queryKey: ["jobs"] }),
        queryClient.invalidateQueries({ queryKey: ["audit", "jobs", jobId] }),
      ]);
    },
  });
}
