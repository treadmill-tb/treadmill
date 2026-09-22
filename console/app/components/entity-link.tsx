import { Link } from "react-router";

/**
 * Short display form of a UUID, matching the CLI: `^` plus its final eight
 * hexadecimal digits. The tail, not the head, because UUIDv7s share their
 * leading timestamp bits with everything created around the same time.
 */
export function shortId(id: string): string {
  return `^${id.replaceAll("-", "").slice(-8)}`;
}

const ROUTES = {
  job: "/jobs",
  host: "/hosts",
  user: "/users",
  imageSet: "/image-sets",
} as const;

export function EntityLink({
  kind,
  id,
  label,
}: {
  kind: keyof typeof ROUTES;
  id: string | null | undefined;
  label?: string;
}) {
  if (id == null) {
    return <span className="muted">—</span>;
  }
  return (
    <Link
      to={`${ROUTES[kind]}/${id}`}
      className={label === undefined ? "mono" : undefined}
      title={id}
    >
      {label ?? shortId(id)}
    </Link>
  );
}
