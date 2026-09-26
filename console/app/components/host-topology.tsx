import { RotateCcw, ZoomIn, ZoomOut } from "lucide-react";
import {
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import {
  TransformComponent,
  TransformWrapper,
  type ReactZoomPanPinchRef,
} from "react-zoom-pan-pinch";

import type {
  DutV2,
  GpioController,
  GpioPin,
  HostSpecV2,
} from "../api/host-spec";
import {
  DebuggerIcon,
  DevBoardIcon,
  GpioControllerIcon,
  PlatformIcon,
} from "../icons";
import { pinDirection, pinSummary } from "./gpio";

const FONT = 12;
const MARGIN = 48;
const BOX_H = 40;
const ADAPTER_H = 54;
const SPACING = 12;
const ROW_GAP = 16;
const COL_GAP = 100;
const WIRE = 12;
const WIRE_PAD = 8;
const BEND = 5;
const LINE_H = 24;
const HOST_W = 180;
const ADAPTER_W = 150;
const DUT_W = 180;
const PAD = 2;
const GLYPHS = 48;

const ADAPTER_X = PAD + HOST_W + COL_GAP;
const ADAPTER_RIGHT = ADAPTER_X + ADAPTER_W;

type Wire = { dut: number; pin: string; spec: GpioPin };

type Slot =
  | {
      kind: "debugger";
      dut: DutV2;
      protocol: string;
      uart: boolean;
      usb: boolean;
    }
  | { kind: "uart" }
  | {
      kind: "controller";
      name: string;
      controller: GpioController;
      wires: Wire[];
      shared: boolean;
    }
  | { kind: "lane"; controller: string; wires: Wire[] }
  | { kind: "title" };

type Placed = Slot & { top: number; height: number };

function uartFolded(dut: DutV2): boolean {
  const serial = dut.debug?.probe.serial;
  return (
    dut.console != null &&
    serial != null &&
    serial !== "" &&
    dut.console.device.includes(serial)
  );
}

function slotHeight(slot: Slot): number {
  switch (slot.kind) {
    case "controller":
      return Math.max(ADAPTER_H, slot.wires.length * WIRE + 2 * WIRE_PAD);
    case "lane":
      return slot.wires.length * WIRE;
    case "uart":
      return LINE_H;
    default:
      return ADAPTER_H;
  }
}

function wireYs(slot: Placed): number[] {
  if (slot.kind !== "controller" && slot.kind !== "lane") return [];
  const offset = (slot.height - slot.wires.length * WIRE) / 2;
  return slot.wires.map((_, k) => slot.top + offset + WIRE / 2 + k * WIRE);
}

function wiresOf(spec: HostSpecV2, controller: string): Wire[] {
  return spec.duts.flatMap((dut, i) =>
    Object.entries(dut.gpio)
      .filter(([, pin]) => pin.controller === controller)
      .map(([pin, spec]) => ({ dut: i, pin, spec })),
  );
}

function layout(spec: HostSpecV2) {
  const rows: Slot[][] = spec.duts.map((dut) => {
    const row: Slot[] = [];
    if (dut.debug != null) {
      const protocol = dut.debug.protocol.toUpperCase();
      const folded = uartFolded(dut);
      row.push({
        kind: "debugger",
        dut,
        protocol,
        uart: folded,
        usb: folded && /usb/i.test(dut.console?.device ?? ""),
      });
    }
    if (dut.console != null && !uartFolded(dut)) row.push({ kind: "uart" });
    return row;
  });
  const above: Slot[][] = spec.duts.map(() => []);
  const below: Slot[][] = spec.duts.map(() => []);
  const gaps: Slot[][] = spec.duts.map(() => []);
  const unused: Slot[] = [];

  for (const [name, controller] of Object.entries(spec.gpio_controllers)) {
    const wires = wiresOf(spec, name);
    const duts = [...new Set(wires.map((w) => w.dut))];
    const first = duts[0];
    if (first === undefined) {
      unused.push({
        kind: "controller",
        name,
        controller,
        wires,
        shared: false,
      });
    } else if (duts.length === 1) {
      rows[first]?.push({
        kind: "controller",
        name,
        controller,
        wires,
        shared: false,
      });
    } else {
      const gap = Math.floor(duts.reduce((a, b) => a + b, 0) / duts.length);
      gaps[gap]?.push({
        kind: "controller",
        name,
        controller,
        wires,
        shared: true,
      });
      for (const dut of duts) {
        const lane: Slot = {
          kind: "lane",
          controller: name,
          wires: wires.filter((w) => w.dut === dut),
        };
        if (dut <= gap) below[dut]?.push(lane);
        else above[dut]?.push(lane);
      }
    }
  }

  const placed: Placed[] = [];
  const dutSpans: { top: number; bottom: number }[] = [];
  const lanes = new Map<string, Placed>();
  let y = PAD;
  const place = (slot: Slot) => {
    const height = slotHeight(slot);
    const p = { ...slot, top: y, height };
    placed.push(p);
    y += height + SPACING;
    return p;
  };

  spec.duts.forEach((_, i) => {
    if (i > 0) y += ROW_GAP;
    const row = [...(above[i] ?? []), ...(rows[i] ?? []), ...(below[i] ?? [])];
    if (row[0]?.kind !== "debugger") row.unshift({ kind: "title" });
    const top = y;
    for (const slot of row) {
      const p = place(slot);
      if (slot.kind === "lane") lanes.set(`${slot.controller}/${i}`, p);
    }
    dutSpans.push({ top, bottom: y - SPACING });
    const gap = gaps[i] ?? [];
    if (gap.length > 0) {
      y += ROW_GAP;
      gap.forEach(place);
    }
  });
  if (unused.length > 0) {
    if (spec.duts.length > 0) y += ROW_GAP;
    unused.forEach(place);
  }

  const bends = Math.max(
    0,
    ...placed.map((slot) => {
      if (slot.kind !== "controller" || !slot.shared) return 0;
      return slot.wires.length;
    }),
  );
  const dutX =
    ADAPTER_RIGHT + Math.max(COL_GAP, 16 + bends * BEND + 24 + GLYPHS);
  const height = Math.max(y - SPACING, PAD + BOX_H) + PAD;
  return { placed, dutSpans, lanes, dutX, height };
}

function fit(text: string, chars: number): string {
  return text.length > chars ? `${text.slice(0, chars - 1)}…` : text;
}

function Node({
  x,
  y,
  width,
  height = BOX_H,
  kind,
  icon,
  overline,
  title,
  subtitle,
  tooltip,
}: {
  x: number;
  y: number;
  width: number;
  height?: number;
  kind?: string;
  icon: ReactNode;
  overline?: string;
  title: string;
  subtitle?: string | null;
  tooltip?: string;
}) {
  const chars = Math.floor((width - 38) / 7);
  const top = kind != null ? y + 14 : y;
  return (
    <g className="node">
      <title>
        {tooltip ??
          [kind, overline, title, subtitle].filter(Boolean).join("\n")}
      </title>
      <rect x={x} y={y} width={width} height={height} rx={6} />
      {kind != null && (
        <text className="kind" x={x + 10} y={y + 14}>
          {kind}
        </text>
      )}
      <g transform={`translate(${x + 10} ${top + 12})`}>{icon}</g>
      {overline != null && (
        <text className="sub" x={x + 32} y={top + 17}>
          {fit(overline, chars + 1)}
        </text>
      )}
      <text x={x + 32} y={overline != null ? top + 31 : top + 17}>
        {fit(title, chars)}
      </text>
      {subtitle != null && (
        <text className="sub" x={x + 32} y={top + 31}>
          {fit(subtitle, chars + 1)}
        </text>
      )}
    </g>
  );
}

function EdgeLabel({
  x,
  y,
  children,
}: {
  x: number;
  y: number;
  children: string;
}) {
  return (
    <text className="edge-label" x={x} y={y}>
      {children}
    </text>
  );
}

function PinLabel({ x, y, wire }: { x: number; y: number; wire: Wire }) {
  const chars = Math.floor((DUT_W - 16) / 6);
  const { label, active } = wire.spec;
  const text = fit(label != null ? `${wire.pin} (${label})` : wire.pin, chars);
  if (active !== "low") {
    return (
      <text className="pin" x={x} y={y + 3.5}>
        {text}
      </text>
    );
  }
  const start = label != null ? wire.pin.length + 2 : 0;
  const end = label != null && text.endsWith(")") ? -1 : undefined;
  return (
    <text className="pin" x={x} y={y + 3.5}>
      {text.slice(0, start)}
      <tspan className="active-low">{text.slice(start, end)}</tspan>
      {end !== undefined && ")"}
    </text>
  );
}

function Inverter({ x, y, wire }: { x: number; y: number; wire: Wire }) {
  switch (pinDirection(wire.spec)) {
    case "out":
      return (
        <g className="glyph">
          <title>Inverter</title>
          <path d={`M${x - 6} ${y - 4}L${x + 2} ${y}L${x - 6} ${y + 4}Z`} />
          <circle cx={x + 4} cy={y} r={2} />
        </g>
      );
    case "in":
      return (
        <g className="glyph">
          <title>Inverter</title>
          <path d={`M${x + 6} ${y - 4}L${x - 2} ${y}L${x + 6} ${y + 4}Z`} />
          <circle cx={x - 4} cy={y} r={2} />
        </g>
      );
    default:
      return (
        <g className="glyph">
          <title>Bidirectional inverter</title>
          <path d={`M${x} ${y - 4}L${x - 8} ${y}L${x} ${y + 4}Z`} />
          <path d={`M${x} ${y - 4}L${x + 8} ${y}L${x} ${y + 4}Z`} />
          <circle cx={x - 10} cy={y} r={2} />
          <circle cx={x + 10} cy={y} r={2} />
        </g>
      );
  }
}

function DriveMark({ x, y, wire }: { x: number; y: number; wire: Wire }) {
  const { drive } = wire.spec;
  if (drive !== "open_drain" && drive !== "open_source") return null;
  const bar = drive === "open_drain" ? y + 5 : y - 5;
  return (
    <g className="glyph">
      <title>{drive === "open_drain" ? "Open drain" : "Open source"}</title>
      <path
        d={`M${x} ${y - 3.5}L${x + 3.5} ${y}L${x} ${y + 3.5}L${x - 3.5} ${y}Z`}
      />
      <path d={`M${x - 3.5} ${bar}H${x + 3.5}`} />
    </g>
  );
}

function Controller({
  slot,
  lanes,
  dutX,
  arrow,
  wireArrow,
}: {
  slot: Extract<Placed, { kind: "controller" }>;
  lanes: Map<string, Placed>;
  dutX: number;
  arrow: string;
  wireArrow: string;
}) {
  const hostRight = PAD + HOST_W;
  const cy = slot.top + ADAPTER_H / 2;
  const starts = wireYs(slot);
  const targets = slot.wires.map((wire) => {
    const lane = lanes.get(`${slot.name}/${wire.dut}`);
    if (lane === undefined) return undefined;
    const k = lane.kind === "lane" ? lane.wires.indexOf(wire) : -1;
    return wireYs(lane)[k];
  });
  const up = slot.wires
    .map((_, k) => k)
    .filter((k) => (targets[k] ?? 0) < (starts[k] ?? 0));
  const down = slot.wires
    .map((_, k) => k)
    .filter((k) => !up.includes(k))
    .reverse();
  const bendOf = (k: number) => {
    const order = up.includes(k) ? up.indexOf(k) : down.indexOf(k);
    return ADAPTER_RIGHT + 16 + order * BEND;
  };

  return (
    <g>
      <path
        className="edge"
        d={`M${hostRight} ${cy}H${ADAPTER_X}`}
        markerEnd={arrow}
      />
      <Node
        x={ADAPTER_X}
        y={slot.top}
        width={ADAPTER_W}
        height={slot.height}
        kind="GPIO controller"
        icon={<GpioControllerIcon size={16} />}
        title={slot.name}
        subtitle={slot.controller.driver}
        tooltip={[
          "GPIO controller",
          slot.name,
          slot.controller.driver,
          ...Object.entries(slot.controller.config).map(
            ([k, v]) => `${k}: ${JSON.stringify(v)}`,
          ),
        ].join("\n")}
      />
      {slot.wires.map((wire, k) => {
        const start = starts[k] ?? cy;
        const target = targets[k];
        const y = target ?? start;
        const d =
          target === undefined
            ? `M${ADAPTER_RIGHT} ${start}H${dutX}`
            : `M${ADAPTER_RIGHT} ${start}H${bendOf(k)}V${target}H${dutX}`;
        const direction = pinDirection(wire.spec);
        return (
          <g key={`${wire.dut}/${wire.pin}`}>
            <title>{pinSummary(wire.pin, wire.spec)}</title>
            <path
              className="wire"
              d={d}
              markerStart={
                direction === "in" || direction === "both"
                  ? wireArrow
                  : undefined
              }
              markerEnd={
                direction === "out" || direction === "both"
                  ? wireArrow
                  : undefined
              }
            />
            {wire.spec.inverted === true && (
              <Inverter x={dutX - 32} y={y} wire={wire} />
            )}
            <DriveMark x={dutX - 14} y={y} wire={wire} />
            <PinLabel x={dutX + 8} y={y} wire={wire} />
          </g>
        );
      })}
    </g>
  );
}

function ZoomView({
  width,
  height,
  children,
}: {
  width: number;
  height: number;
  children: ReactNode;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const transform = useRef<ReactZoomPanPinchRef>(null);
  const [view, setView] = useState<{
    initial: number;
    fit: number;
    margin: number;
    x: number;
  } | null>(null);
  const [pannable, setPannable] = useState(false);

  const updatePannable = (zoom: ReactZoomPanPinchRef) => {
    const { wrapperComponent: wrapper, contentComponent: content } =
      zoom.instance;
    if (wrapper === null || content === null) return;
    const { scale } = zoom.state;
    setPannable(
      content.offsetWidth * scale > wrapper.clientWidth + 1 ||
        content.offsetHeight * scale > wrapper.clientHeight + 1,
    );
  };

  useLayoutEffect(() => {
    const el = ref.current;
    if (el === null) return;
    const measure = () => {
      const font = parseFloat(getComputedStyle(el).fontSize) / FONT;
      const fit = el.clientWidth / width;
      const initial = Math.min(font, Math.max(fit, 0.75 * font));
      const margin = Math.min(MARGIN, el.clientWidth * 0.05);
      const x = Math.max(0, (el.clientWidth - width * initial) / 2 - margin);
      setView((prev) =>
        prev?.initial === initial &&
        prev.fit === fit &&
        prev.margin === margin &&
        prev.x === x
          ? prev
          : { initial, fit, margin, x },
      );
      if (transform.current !== null) updatePannable(transform.current);
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    return () => observer.disconnect();
  }, [width]);

  return (
    <div
      ref={ref}
      className={pannable ? "topology pannable" : "topology"}
      style={
        view === null
          ? undefined
          : {
              height: `min(${height * view.initial + 2 * view.margin}px, 70vh)`,
            }
      }
    >
      {view !== null && (
        <TransformWrapper
          key={`${view.initial}/${view.fit}/${view.x}`}
          ref={transform}
          initialScale={view.initial}
          initialPositionX={view.x}
          initialPositionY={0}
          minScale={Math.min(view.fit, view.initial)}
          maxScale={view.initial * 4}
          centerZoomedOut
          wheel={{ wheelDisabled: true }}
          trackPadPanning={{ disabled: true }}
          panning={{ disabled: !pannable }}
          doubleClick={{ disabled: true }}
          onInit={updatePannable}
          onTransform={updatePannable}
        >
          {({ zoomIn, zoomOut, setTransform }) => (
            <>
              <div className="topology-controls">
                <button
                  type="button"
                  className="icon-btn"
                  aria-label="Zoom in"
                  onClick={() => void zoomIn()}
                >
                  <ZoomIn size={16} />
                </button>
                <button
                  type="button"
                  className="icon-btn"
                  aria-label="Zoom out"
                  onClick={() => void zoomOut()}
                >
                  <ZoomOut size={16} />
                </button>
                <button
                  type="button"
                  className="icon-btn"
                  aria-label="Reset zoom"
                  onClick={() => void setTransform(view.x, 0, view.initial)}
                >
                  <RotateCcw size={16} />
                </button>
              </div>
              <TransformComponent
                wrapperStyle={{ width: "100%", height: "100%" }}
              >
                <div style={{ padding: view.margin / view.initial }}>
                  {children}
                </div>
              </TransformComponent>
            </>
          )}
        </TransformWrapper>
      )}
    </div>
  );
}

export function HostTopology({ spec }: { spec: HostSpecV2 }) {
  const id = useId().replace(/[^a-zA-Z0-9]/g, "");
  const arrow = `url(#arrow-${id})`;
  const wireArrow = `url(#wire-${id})`;
  const { placed, dutSpans, lanes, dutX, height } = layout(spec);
  const width = dutX + DUT_W + PAD;
  const hostRight = PAD + HOST_W;
  const platform = spec.platform;
  const model =
    platform.kind === "physical" ? platform.model : platform.hypervisor;

  return (
    <ZoomView width={width} height={height}>
      <svg
        width={width}
        height={height}
        viewBox={`0 0 ${width} ${height}`}
        role="img"
        aria-label={`Wiring of ${spec.name}`}
        style={{ fontSize: FONT }}
      >
        <defs>
          <marker
            id={`arrow-${id}`}
            viewBox="0 0 8 8"
            refX={8}
            refY={4}
            markerWidth={8}
            markerHeight={8}
            orient="auto"
          >
            <path d="M0 0L8 4L0 8Z" />
          </marker>
          <marker
            id={`wire-${id}`}
            viewBox="0 0 8 8"
            refX={8}
            refY={4}
            markerWidth={5}
            markerHeight={5}
            orient="auto-start-reverse"
          >
            <path d="M0 0L8 4L0 8Z" />
          </marker>
        </defs>

        <Node
          x={PAD}
          y={PAD}
          width={HOST_W}
          height={height - 2 * PAD}
          icon={<PlatformIcon platform={platform} size={16} />}
          title={spec.name}
          subtitle={model}
        />

        {spec.duts.map((dut, i) => {
          const span = dutSpans[i];
          if (span === undefined) return null;
          return (
            <Node
              key={i}
              x={dutX}
              y={span.top}
              width={DUT_W}
              height={span.bottom - span.top}
              kind="Target (Device Under Test)"
              icon={<DevBoardIcon size={16} />}
              overline={dut.vendor}
              title={dut.name ?? dut.board}
              tooltip={[
                "Target (Device Under Test)",
                dut.vendor,
                dut.name,
                dut.board,
              ]
                .filter(Boolean)
                .join("\n")}
            />
          );
        })}

        {placed.map((slot, i) => {
          const cy = slot.top + slot.height / 2;
          switch (slot.kind) {
            case "debugger": {
              const probe = slot.dut.debug?.probe;
              const links = slot.uart
                ? [
                    { label: slot.protocol, y: slot.top + 19 },
                    { label: "UART", y: slot.top + 43 },
                  ]
                : [{ label: slot.protocol, y: cy }];
              return (
                <g key={i}>
                  <path
                    className="edge"
                    d={`M${hostRight} ${cy}H${ADAPTER_X}`}
                    markerEnd={arrow}
                  />
                  <Node
                    x={ADAPTER_X}
                    y={slot.top}
                    width={ADAPTER_W}
                    height={ADAPTER_H}
                    kind="Debugger"
                    icon={<DebuggerIcon usb={slot.usb} size={16} />}
                    overline={probe?.vendor}
                    title={probe?.model ?? ""}
                    tooltip={[
                      "Debugger",
                      probe?.vendor,
                      probe?.model,
                      probe?.serial,
                    ]
                      .filter(Boolean)
                      .join("\n")}
                  />
                  {links.map(({ label, y }) => (
                    <g key={label}>
                      <path
                        className="edge"
                        d={`M${ADAPTER_RIGHT} ${y}H${dutX}`}
                        markerEnd={arrow}
                      />
                      <EdgeLabel x={ADAPTER_RIGHT + 8} y={y - 6}>
                        {label}
                      </EdgeLabel>
                    </g>
                  ))}
                </g>
              );
            }
            case "uart":
              return (
                <g key={i}>
                  <path
                    className="edge"
                    d={`M${hostRight} ${cy}H${dutX}`}
                    markerEnd={arrow}
                  />
                  <EdgeLabel x={ADAPTER_RIGHT + 8} y={cy - 6}>
                    UART
                  </EdgeLabel>
                </g>
              );
            case "controller":
              return (
                <Controller
                  key={i}
                  slot={slot}
                  lanes={lanes}
                  dutX={dutX}
                  arrow={arrow}
                  wireArrow={wireArrow}
                />
              );
            default:
              return null;
          }
        })}
      </svg>
    </ZoomView>
  );
}
