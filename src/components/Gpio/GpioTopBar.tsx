import { Cpu, Power, RefreshCw, RotateCcw } from "lucide-react";

interface GpioTopBarProps {
  otg: boolean;
  busy: boolean;
  onToggleOtg: () => void;
  onResetAll: () => void;
  onRefresh: () => void;
  pollIntervalMs: number;
  onPollIntervalChange: (value: number) => void;
  /** Minimum allowed poll interval — BLE bumps this to 500 ms. */
  minPollMs: number;
  /** Tooltip shown over the slider when its floor is being clamped. */
  pollFloorReason: string | null;
}

export function GpioTopBar({
  otg,
  busy,
  onToggleOtg,
  onResetAll,
  onRefresh,
  pollIntervalMs,
  onPollIntervalChange,
  minPollMs,
  pollFloorReason,
}: GpioTopBarProps) {
  return (
    <header className="shrink-0 border-b border-border-subtle bg-panel">
      <div className="flex items-center gap-2 px-3 py-2 flex-wrap">
        <Cpu size={14} className="text-accent" />
        <h2 className="text-xs font-medium text-primary">GPIO</h2>

        {/* OTG chip — mirror of the pin-1 switch. */}
        <button
          type="button"
          onClick={onToggleOtg}
          disabled={busy}
          aria-pressed={otg}
          title={otg ? "Turn OTG +5 V off" : "Turn OTG +5 V on"}
          className={[
            "ml-2 inline-flex items-center gap-1.5 px-2 py-1 text-[11px] rounded border transition-colors disabled:opacity-50 disabled:cursor-not-allowed",
            otg
              ? "border-success/40 bg-success/10 text-success hover:bg-success/20"
              : "border-border-subtle bg-surface text-secondary hover:text-primary hover:bg-elevated",
          ].join(" ")}
        >
          <Power size={11} />
          <span>OTG +5 V</span>
          <span className="font-mono tabular-nums">{otg ? "ON" : "OFF"}</span>
        </button>

        <div className="flex-1" />

        {/* Poll interval slider. */}
        <label
          className="flex items-center gap-2 text-[11px] text-secondary"
          title={pollFloorReason ?? "Live read poll interval"}
        >
          <span className="uppercase tracking-wide text-muted">poll</span>
          <input
            type="range"
            min={minPollMs}
            max={1000}
            step={50}
            value={pollIntervalMs}
            onChange={(e) => onPollIntervalChange(Number(e.target.value))}
            className="w-28 accent-accent"
          />
          <span className="font-mono tabular-nums w-12 text-right text-primary">
            {pollIntervalMs} ms
          </span>
        </label>

        <button
          type="button"
          onClick={onResetAll}
          disabled={busy}
          title="Set every controllable pin back to INPUT with no pull"
          className="flex items-center gap-1 px-2 py-1 text-[11px] text-secondary hover:text-primary border border-border-subtle rounded hover:bg-surface/60 disabled:opacity-40 disabled:cursor-not-allowed"
        >
          <RotateCcw size={11} />
          Reset all
        </button>

        <button
          type="button"
          onClick={onRefresh}
          disabled={busy}
          title="Re-read every pin and OTG state"
          className="flex items-center gap-1 px-2 py-1 text-[11px] text-secondary hover:text-primary border border-border-subtle rounded hover:bg-surface/60 disabled:opacity-40 disabled:cursor-not-allowed"
        >
          <RefreshCw size={11} className={busy ? "animate-spin" : ""} />
          Refresh
        </button>
      </div>
      {pollFloorReason && (
        <div className="px-3 pb-2 text-[10px] text-muted">{pollFloorReason}</div>
      )}
    </header>
  );
}
