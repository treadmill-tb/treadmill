import type { GpioDrive, GpioPin } from "../api/host-spec";

export type PinDirection = "out" | "in" | "both" | "none";

export function pinDirection(pin: GpioPin): PinDirection {
  const output = pin.modes.includes("digital_out");
  const input = pin.modes.includes("digital_in");
  if (output && input) return "both";
  if (output) return "out";
  if (input) return "in";
  return "none";
}

export const DRIVE_NAMES: Record<GpioDrive, string> = {
  push_pull: "push-pull",
  open_drain: "open drain",
  open_source: "open source",
};

export function pinSummary(name: string, pin: GpioPin): string {
  return [
    pin.label != null ? `${name} (${pin.label})` : name,
    `modes: ${pin.modes.join(", ")}`,
    pin.active != null ? `active ${pin.active}` : null,
    pin.inverted === true ? "inverted" : null,
    pin.drive != null ? `drive: ${DRIVE_NAMES[pin.drive]}` : null,
    pin.note,
  ]
    .filter(Boolean)
    .join("\n");
}
