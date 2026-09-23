import { useQueryClient } from "@tanstack/react-query";

import { client } from "../api/client";
import { ApiError } from "../api/errors";
import { applyAccess, planAccess, type Variant } from "../api/images";
import type { components } from "../api/schema";
import { ShareDialog, type ApplyAccess, type Role } from "./share-dialog";

type ImageSetPermission = components["schemas"]["ImageSetPermission"];
type ImageSetGrantInfo = components["schemas"]["ImageSetGrantInfo"];

const ROLES: Role<ImageSetPermission>[] = [
  {
    label: "Can use",
    detail: "run jobs with it",
    permissions: ["use"],
  },
  {
    label: "Can edit",
    detail: "also publish versions and share it",
    permissions: ["use", "manage"],
  },
];

const PUBLIC_LEVELS: Role<ImageSetPermission>[] = [
  {
    label: "Public",
    permissions: ["use"],
  },
];

/** Giving `use` first shares the builds of the current variants. */
export function ImageShareDialog({
  open,
  onClose,
  setId,
  title,
  ownerId,
  grants,
  grantsError,
  variants,
}: {
  open: boolean;
  onClose: () => void;
  setId: string;
  title: string;
  ownerId: string | null | undefined;
  grants: ImageSetGrantInfo[] | undefined;
  grantsError: unknown;
  /** The latest version's variants. */
  variants: Variant[];
}) {
  const queryClient = useQueryClient();

  const apply: ApplyAccess<ImageSetPermission> = async (
    subject,
    permissions,
    force,
  ) => {
    const current = (grants ?? [])
      .filter((g) => g.subject_id === subject)
      .map((g) => g.permission);
    const adds = permissions.filter((p) => !current.includes(p));
    const removes = current.filter((p) => !permissions.includes(p));

    if (adds.includes("use")) {
      const plan = await planAccess(variants, [subject]);
      if (plan.blocked.length > 0 && !force) {
        return { blocked: plan.blocked.map((v) => v.platform_profile) };
      }
      await applyAccess(plan);
    }
    try {
      for (const permission of adds) {
        const r = await client.POST("/image-sets/{id}/grants", {
          params: { path: { id: setId } },
          body: { subject_id: subject, permission },
        });
        if (!r.response.ok) throw new ApiError(r.response.status, r.error);
      }
      for (const permission of removes) {
        const r = await client.DELETE(
          "/image-sets/{id}/grants/{subject_id}/{permission}",
          {
            params: { path: { id: setId, subject_id: subject, permission } },
          },
        );
        if (!r.response.ok) throw new ApiError(r.response.status, r.error);
      }
    } finally {
      await Promise.all([
        queryClient.invalidateQueries({
          queryKey: ["get", "/image-sets/{id}/grants"],
        }),
        queryClient.invalidateQueries({
          queryKey: ["get", "/image-sets/{id}/generations/{n}"],
        }),
        queryClient.invalidateQueries({ queryKey: ["get", "/image-sets"] }),
        queryClient.invalidateQueries({
          queryKey: ["audit", "image-sets", setId],
        }),
      ]);
    }
    return null;
  };

  return (
    <ShareDialog
      open={open}
      onClose={onClose}
      title={title}
      ownerId={ownerId}
      grants={grants}
      grantsError={grantsError}
      roles={ROLES}
      publicLevels={PUBLIC_LEVELS}
      apply={apply}
    />
  );
}
