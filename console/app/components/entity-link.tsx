import type { LucideIcon } from "lucide-react";
import { Link } from "react-router";

/**
 * Short display form of a UUID, matching the CLI: `^` plus its final eight
 * hexadecimal digits. The tail, not the head, because UUIDv7s share their
 * leading timestamp bits with everything created around the same time.
 */
export function shortId(id: string): string {
  return `^${id.replaceAll("-", "").slice(-8)}`;
}

/** A UUID in its short form, the full one on hover. */
export function ShortId({ id }: { id: string }) {
  return (
    <span className="short-id" title={id}>
      {shortId(id)}
    </span>
  );
}

const ROUTES = {
  job: "/jobs",
  host: "/hosts",
  user: "/users",
  imageSet: "/images",
} as const;

export function EntityLink({
  kind,
  id,
  label,
  icon: Icon,
}: {
  kind: keyof typeof ROUTES;
  id: string | null | undefined;
  label?: string;
  /** Marks the link out as one, for where plain text surrounds it. */
  icon?: LucideIcon;
}) {
  if (id == null) {
    return <span className="muted">—</span>;
  }
  return (
    <Link
      to={`${ROUTES[kind]}/${id}`}
      className={
        [label === undefined && "short-id", Icon !== undefined && "icon-link"]
          .filter(Boolean)
          .join(" ") || undefined
      }
      title={id}
    >
      {Icon !== undefined && <Icon size={14} aria-hidden="true" />}
      {label ?? shortId(id)}
    </Link>
  );
}
