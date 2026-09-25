import { Box, Server, Usb } from "lucide-react";
import type { ComponentType, SVGProps } from "react";

import type { components } from "../api/schema";
import DebuggerSvg from "./debugger.svg?react";
import DevBoardSvg from "./dev-board.svg?react";
import GpioMuxSvg from "./gpio-mux.svg?react";
import RaspberryPiSvg from "./raspberry-pi.svg?react";

type PlatformSummary = components["schemas"]["PlatformSummary"];
type IconProps = Omit<SVGProps<SVGSVGElement>, "ref"> & { size?: number };

function sized(Svg: ComponentType<SVGProps<SVGSVGElement>>) {
  return function SizedIcon({ size = 24, ...props }: IconProps) {
    return <Svg width={size} height={size} {...props} />;
  };
}

const RaspberryPi = sized(RaspberryPiSvg);
const Debugger = sized(DebuggerSvg);

export const DevBoardIcon = sized(DevBoardSvg);
export const GpioControllerIcon = sized(GpioMuxSvg);

export function DebuggerIcon({ usb, ...props }: IconProps & { usb: boolean }) {
  return usb ? <Usb {...props} /> : <Debugger {...props} />;
}

export function PlatformIcon({
  platform,
  ...props
}: IconProps & { platform: Pick<PlatformSummary, "kind" | "model"> }) {
  if (platform.model != null && /raspberry pi/i.test(platform.model))
    return <RaspberryPi {...props} />;
  if (platform.kind === "virtual") return <Box {...props} />;
  return <Server {...props} />;
}
