import type { DebugProbe, DutV2, GpioController } from "../api/host-spec";

export function consoleOnProbe(dut: DutV2): boolean {
  const serial = dut.debug?.probe.serial;
  return (
    dut.console != null &&
    serial != null &&
    serial !== "" &&
    dut.console.device.includes(serial)
  );
}

export function usbProbe(dut: DutV2): boolean {
  return consoleOnProbe(dut) && /usb/i.test(dut.console?.device ?? "");
}

export function probeLink(probe: DebugProbe): string | undefined {
  if (/^(st-link|j-link)/i.test(probe.model)) return "USB";
  if (
    /^raspberry pi$/i.test(probe.vendor) &&
    /^debug probe$/i.test(probe.model)
  )
    return "USB";
  return undefined;
}

export function controllerLink(controller: GpioController): string | undefined {
  return controller.config.label === "pinctrl-rp1" ? "PCIe" : undefined;
}

export function dutLinks(dut: DutV2): string[] {
  const links: string[] = [];
  if (dut.console != null && !consoleOnProbe(dut)) links.push("UART");
  if (dut.connectivity.includes("usb")) links.push("USB");
  if (dut.connectivity.includes("ethernet")) links.push("Ethernet");
  return links;
}
