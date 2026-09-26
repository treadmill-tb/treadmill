import { ArrowLeft, ArrowLeftRight, ArrowRight } from "lucide-react";

import type { DutV2, GpioController, GpioPin } from "../api/host-spec";
import { DevBoardIcon } from "../icons";
import { CopyButton } from "./copy-button";
import { DRIVE_NAMES, pinDirection } from "./gpio";

const DIRECTION_ICONS = {
  out: ArrowRight,
  in: ArrowLeft,
  both: ArrowLeftRight,
};

function Direction({ pin }: { pin: GpioPin }) {
  const modes = pin.modes.join(", ");
  const direction = pinDirection(pin);
  const known = pin.modes.every(
    (mode) => mode === "digital_in" || mode === "digital_out",
  );
  if (direction === "none" || !known) {
    return <span className="mono">{modes}</span>;
  }
  const Icon = DIRECTION_ICONS[direction];
  return (
    <span className="pin-direction" title={modes}>
      <Icon size={16} aria-label={modes} />
    </span>
  );
}

function byController(gpio: DutV2["gpio"]): [string, [string, GpioPin][]][] {
  const groups = new Map<string, [string, GpioPin][]>();
  for (const [name, pin] of Object.entries(gpio)) {
    const group = groups.get(pin.controller) ?? [];
    group.push([name, pin]);
    groups.set(pin.controller, group);
  }
  return [...groups];
}

export function DutCard({
  dut,
  controllers,
}: {
  dut: DutV2;
  controllers: Record<string, GpioController>;
}) {
  const probe = dut.debug?.probe;
  const groups = byController(dut.gpio);
  const labels = Object.entries(dut.labels);
  return (
    <section className="card dut-card">
      <h3 className="host-name">
        <DevBoardIcon size={18} aria-hidden="true" />
        {dut.name ?? dut.board}
      </h3>
      <p className="host-facts">
        {[
          `${dut.vendor} ${dut.board}`,
          dut.serial != null ? `serial ${dut.serial}` : null,
          dut.arch.join(", "),
          dut.connectivity.join(", "),
        ]
          .filter(Boolean)
          .join(" · ")}
      </p>
      {(dut.debug != null || dut.console != null) && (
        <dl className="props">
          {dut.debug != null && probe != null && (
            <>
              <dt>Debugger</dt>
              <dd>
                {probe.vendor} {probe.model} · {dut.debug.protocol}
                {probe.serial != null && (
                  <>
                    {" · "}
                    <span className="mono">{probe.serial}</span>
                  </>
                )}
              </dd>
            </>
          )}
          {dut.console != null && (
            <>
              <dt>Console</dt>
              <dd className="console-device">
                <span>{dut.console.baud} baud ·</span>
                <span className="mono ellipsis" title={dut.console.device}>
                  {dut.console.device}
                </span>
                <CopyButton
                  value={dut.console.device}
                  label="Copy console device"
                />
              </dd>
            </>
          )}
        </dl>
      )}
      {groups.length > 0 && (
        <>
          <h4>GPIO</h4>
          <div className="pin-table">
            <table>
              <thead>
                <tr>
                  <th>Pin</th>
                  <th>Direction</th>
                  <th>Wiring</th>
                </tr>
              </thead>
              {groups.map(([controller, pins]) => (
                <tbody key={controller}>
                  <tr className="pin-group">
                    <th colSpan={3} scope="colgroup">
                      <span className="mono">{controller}</span>
                      {controllers[controller] != null &&
                        ` · ${controllers[controller].driver}`}
                    </th>
                  </tr>
                  {pins.map(([name, pin]) => (
                    <tr key={name}>
                      <td>
                        <div className="mono">{name}</div>
                        {pin.label != null && (
                          <div
                            className={
                              pin.active === "low"
                                ? "muted active-low"
                                : "muted"
                            }
                            title={
                              pin.active === "low" ? "Active low" : undefined
                            }
                          >
                            {pin.label}
                          </div>
                        )}
                      </td>
                      <td>
                        <Direction pin={pin} />
                      </td>
                      <td>
                        {[
                          pin.active != null ? `active ${pin.active}` : null,
                          pin.inverted === true ? "inverted" : null,
                          pin.drive != null ? DRIVE_NAMES[pin.drive] : null,
                        ]
                          .filter((chip) => chip != null)
                          .map((chip) => (
                            <span key={chip} className="chip">
                              {chip}
                            </span>
                          ))}
                        {pin.note != null && (
                          <div className="muted">{pin.note}</div>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              ))}
            </table>
          </div>
        </>
      )}
      {labels.length > 0 && (
        <p>
          {labels.map(([key, value]) => (
            <span key={key} className="chip mono">
              {key}={value}
            </span>
          ))}
        </p>
      )}
    </section>
  );
}
