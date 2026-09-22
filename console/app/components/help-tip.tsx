import { CircleQuestionMark } from "lucide-react";
import {
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";

/** Keeps the bubble this far from the window's edges. */
const EDGE_MARGIN_PX = 8;

/**
 * A small "?" that explains the element next to it. A mouse shows the text
 * while hovering; a tap or click (or Enter) pins it open until the next tap
 * elsewhere or Escape — which is also how touch screens, having no hover,
 * reach it at all.
 */
export function HelpTip({
  label = "Help",
  children,
}: {
  /** The button's accessible name, e.g. "About this field". */
  label?: string;
  children: ReactNode;
}) {
  const [pinned, setPinned] = useState(false);
  const [hovered, setHovered] = useState(false);
  const wrapRef = useRef<HTMLSpanElement | null>(null);
  const bubbleRef = useRef<HTMLSpanElement | null>(null);
  const id = useId();
  const shown = pinned || hovered;

  // A pinned bubble closes on a tap outside it or on Escape.
  useEffect(() => {
    if (!pinned) return;
    const onPointer = (e: PointerEvent) => {
      if (!wrapRef.current?.contains(e.target as Node)) setPinned(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setPinned(false);
    };
    document.addEventListener("pointerdown", onPointer);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onPointer);
      document.removeEventListener("keydown", onKey);
    };
  }, [pinned]);

  // Centred under the icon by default, then nudged back inside the window
  // if that would cut it off (an icon near a phone's edge, say).
  useLayoutEffect(() => {
    const bubble = bubbleRef.current;
    if (!shown || bubble === null) return;
    bubble.style.setProperty("--shift", "0px");
    const rect = bubble.getBoundingClientRect();
    const right = window.innerWidth - EDGE_MARGIN_PX;
    const shift =
      rect.left < EDGE_MARGIN_PX
        ? EDGE_MARGIN_PX - rect.left
        : rect.right > right
          ? right - rect.right
          : 0;
    bubble.style.setProperty("--shift", `${shift}px`);
  }, [shown]);

  return (
    <span
      ref={wrapRef}
      className="help-tip"
      // Only a real mouse hovers: a touch emulates the enter but never the
      // leave, which would keep the bubble stuck open.
      onPointerEnter={(e) => e.pointerType === "mouse" && setHovered(true)}
      onPointerLeave={(e) => e.pointerType === "mouse" && setHovered(false)}
    >
      <button
        type="button"
        aria-label={label}
        aria-expanded={shown}
        aria-controls={id}
        onClick={() => setPinned((p) => !p)}
      >
        <CircleQuestionMark size={16} aria-hidden="true" />
      </button>
      <span
        ref={bubbleRef}
        id={id}
        role="note"
        className="help-tip-bubble"
        hidden={!shown}
      >
        {children}
      </span>
    </span>
  );
}
