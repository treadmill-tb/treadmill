import { Link } from "react-router";

export function shortId(id: string): string {
  return id.slice(0, 8);
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
    <Link to={`${ROUTES[kind]}/${id}`} className="mono" title={id}>
      {label ?? shortId(id)}
    </Link>
  );
}
