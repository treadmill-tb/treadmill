import type { DutV2 } from "../api/host-spec";
import { DevBoardIcon } from "../icons";

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
                <th>Controller</th>
                <th>Modes</th>
              </tr>
            </thead>
            <tbody>
              {pins.map(([name, pin]) => (
                <tr key={name}>
                  <td className="mono">{name}</td>
                  <td>{pin.label ?? <span className="muted">—</span>}</td>
                  <td className="mono">
                    {pin.controller}{" "}
                    <span className="muted">{formatConfig(pin.config)}</span>
                  </td>
                  <td className="mono">{pin.modes.join(", ")}</td>
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
