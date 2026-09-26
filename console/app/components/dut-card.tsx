import type { DutV2 } from "../api/host-spec";
import { DevBoardIcon } from "../icons";
import { DRIVE_NAMES } from "./gpio";

function formatConfig(config: Record<string, unknown>): string {
  return Object.entries(config)
    .map(([k, v]) => `${k}=${typeof v === "string" ? v : JSON.stringify(v)}`)
    .join(" ");
}

export function DutCard({ dut }: { dut: DutV2 }) {
  const probe = dut.debug?.probe;
  const pins = Object.entries(dut.gpio);
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
              <dd>
                <span className="mono">{dut.console.device}</span> ·{" "}
                {dut.console.baud} baud
              </dd>
            </>
          )}
        </dl>
      )}
      {pins.length > 0 && (
        <div className="overflow-auto">
          <table>
            <thead>
              <tr>
                <th>Pin</th>
                <th>Label</th>
                <th>Modes</th>
                <th>Active</th>
                <th>Wiring</th>
                <th>Controller</th>
              </tr>
            </thead>
            <tbody>
              {pins.map(([name, pin]) => (
                <tr key={name}>
                  <td className="mono">{name}</td>
                  <td>
                    {pin.label != null ? (
                      <span
                        className={
                          pin.active === "low" ? "active-low" : undefined
                        }
                        title={pin.active === "low" ? "Active low" : undefined}
                      >
                        {pin.label}
                      </span>
                    ) : (
                      <span className="muted">—</span>
                    )}
                  </td>
                  <td className="mono">{pin.modes.join(", ")}</td>
                  <td>{pin.active ?? <span className="muted">—</span>}</td>
                  <td>
                    {[
                      pin.inverted === true ? "inverted" : null,
                      pin.drive != null ? DRIVE_NAMES[pin.drive] : null,
                    ]
                      .filter(Boolean)
                      .join(", ") || <span className="muted">—</span>}
                    {pin.note != null && (
                      <div className="muted">{pin.note}</div>
                    )}
                  </td>
                  <td className="mono">
                    {pin.controller}{" "}
                    <span className="muted">{formatConfig(pin.config)}</span>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
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
