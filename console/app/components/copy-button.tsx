import { Check, Copy } from "lucide-react";
import { useEffect, useState } from "react";

/** How long the button shows its "copied" check before reverting. */
const CONFIRM_MS = 1500;

/** Icon button copying `value` to the clipboard, confirming with a check. */
export function CopyButton({ value, label }: { value: string; label: string }) {
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    if (!copied) return;
    const timer = setTimeout(() => setCopied(false), CONFIRM_MS);
    return () => clearTimeout(timer);
  }, [copied]);

  return (
    <button
      type="button"
      className="copy-btn"
      title={copied ? "Copied" : label}
      aria-label={label}
      onClick={() => {
        navigator.clipboard.writeText(value).then(
          () => setCopied(true),
          () => {},
        );
      }}
    >
      {copied ? (
        <Check size={14} aria-hidden="true" />
      ) : (
        <Copy size={14} aria-hidden="true" />
      )}
    </button>
  );
}
