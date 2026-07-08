export function Tags({ tags }: { tags: readonly string[] }) {
  if (tags.length === 0) {
    return <span className="muted">—</span>;
  }
  return (
    <>
      {tags.map((t) => (
        <span key={t} className="tag">
          {t}
        </span>
      ))}
    </>
  );
}
