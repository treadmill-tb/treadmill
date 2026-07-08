const UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
  ["year", 365 * 24 * 3600],
  ["month", 30 * 24 * 3600],
  ["day", 24 * 3600],
  ["hour", 3600],
  ["minute", 60],
  ["second", 1],
];

const fmt = new Intl.RelativeTimeFormat("en", { numeric: "auto" });

function relative(iso: string): string {
  const secs = (Date.parse(iso) - Date.now()) / 1000;
  for (const [unit, size] of UNITS) {
    if (Math.abs(secs) >= size || unit === "second") {
      return fmt.format(Math.trunc(secs / size), unit);
    }
  }
  return iso;
}

export function RelTime({ iso }: { iso: string | null | undefined }) {
  if (iso == null) {
    return <span className="muted">—</span>;
  }
  return (
    <time dateTime={iso} title={iso}>
      {relative(iso)}
    </time>
  );
}
