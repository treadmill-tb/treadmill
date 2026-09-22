import { CopyButton } from "./copy-button";

export function Digest({ digest }: { digest: string | null | undefined }) {
  if (digest == null) {
    return <span className="muted">—</span>;
  }
  const short = digest.startsWith("sha256:")
    ? `sha256:${digest.slice(7, 7 + 12)}`
    : digest.slice(0, 19);
  return (
    <span className="mono" title={digest}>
      {short}… <CopyButton value={digest} label="Copy full digest" />
    </span>
  );
}
