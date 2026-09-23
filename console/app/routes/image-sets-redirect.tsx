import { Navigate, useParams } from "react-router";

export default function ImageSetsRedirect() {
  const { id, n } = useParams();
  const to =
    id === undefined
      ? "/images"
      : n === undefined
        ? `/images/${id}`
        : `/images/${id}/versions/${n}`;
  return <Navigate to={to} replace />;
}
