import { X } from "lucide-react";
import { useEffect, useRef, type ReactNode } from "react";

/**
 * A modal on the native `<dialog>`, which brings the backdrop, focus trapping
 * and Escape-to-close; Pico styles the `<article>` inside it.
 *
 * `onClose` fires however the dialog is dismissed: Escape, the close button, or
 * a click on the backdrop.
 */
export function Dialog({
  open,
  onClose,
  title,
  children,
  footer,
}: {
  open: boolean;
  onClose: () => void;
  title: ReactNode;
  children: ReactNode;
  footer?: ReactNode;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const dialog = ref.current;
    if (dialog === null) return;
    if (open && !dialog.open) dialog.showModal();
    if (!open && dialog.open) dialog.close();
  }, [open]);

  return (
    <dialog
      ref={ref}
      onClose={onClose}
      // The article fills the dialog's content box, so a click landing on the
      // dialog itself is one on the backdrop around it.
      onClick={(e) => e.target === ref.current && onClose()}
    >
      {open && (
        <article>
          <header className="dialog-head">
            <h3>{title}</h3>
            <button
              type="button"
              className="icon-btn"
              aria-label="Close"
              onClick={onClose}
            >
              <X size={18} aria-hidden="true" />
            </button>
          </header>
          {children}
          {footer !== undefined && <footer>{footer}</footer>}
        </article>
      )}
    </dialog>
  );
}

/** A yes/no question, confirmed only by its explicit button. */
export function ConfirmDialog({
  open,
  title,
  children,
  confirmLabel,
  danger = false,
  onConfirm,
  onCancel,
}: {
  open: boolean;
  title: ReactNode;
  children: ReactNode;
  confirmLabel: string;
  danger?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  return (
    <Dialog
      open={open}
      onClose={onCancel}
      title={title}
      footer={
        <>
          <button type="button" onClick={onCancel}>
            Cancel
          </button>
          <button
            type="button"
            className={danger ? "danger" : "primary"}
            onClick={onConfirm}
          >
            {confirmLabel}
          </button>
        </>
      }
    >
      {children}
    </Dialog>
  );
}
