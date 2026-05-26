import { HEADER_PINS, type HeaderPin, type PinKind } from "../../types/gpio";

/**
 * Live state used to draw the small status chip next to each RPC pin row.
 * Looked up by pin name (e.g. "PA7"). Missing entries render no chip.
 */
export interface PinChipMap {
  [name: string]: { mode: "input" | "output"; value: 0 | 1 } | undefined;
}

interface PinColumnProps {
  selectedPin: number;
  onSelect: (pinNumber: number) => void;
  /** Live mode/value for each of the 8 RPC pins. */
  chips: PinChipMap;
  /** Whether the device is connected — drives chip visibility. */
  connected: boolean;
  /** Current OTG state — drives the chip on pin 1. */
  otg: boolean;
}

/**
 * Left-pane "header illustration": a vertical strip of all 18 GPIO header
 * pins in the order they appear on the physical Flipper.
 *
 * Each row is clickable; the parent view stores the selected pin and renders
 * the matching detail card in the right pane.
 */
export function PinColumn({
  selectedPin,
  onSelect,
  chips,
  connected,
  otg,
}: PinColumnProps) {
  return (
    <div className="h-full overflow-y-auto">
      <ul className="flex flex-col gap-0.5 p-2">
        {HEADER_PINS.map((pin) => (
          <PinRow
            key={pin.number}
            pin={pin}
            selected={selectedPin === pin.number}
            onSelect={() => onSelect(pin.number)}
            chip={
              pin.kind === "gpio"
                ? chips[pin.name]
                : pin.number === 1 && connected
                  ? { mode: "output", value: otg ? 1 : 0 }
                  : undefined
            }
            isOtg={pin.number === 1}
          />
        ))}
      </ul>
    </div>
  );
}

function PinRow({
  pin,
  selected,
  onSelect,
  chip,
  isOtg,
}: {
  pin: HeaderPin;
  selected: boolean;
  onSelect: () => void;
  chip: { mode: "input" | "output"; value: 0 | 1 } | undefined;
  isOtg: boolean;
}) {
  const stateClasses = selected
    ? "bg-surface/60 text-primary"
    : "hover:bg-surface/40 text-secondary";

  return (
    <li>
      <button
        type="button"
        onClick={onSelect}
        aria-current={selected ? "true" : undefined}
        className={[
          "relative w-full flex items-center gap-2 px-2 py-1.5 rounded-md transition-colors text-left",
          stateClasses,
        ].join(" ")}
      >
        {selected && (
          <span
            aria-hidden
            className="absolute -left-2 top-1.5 bottom-1.5 w-[2px] rounded-r bg-accent"
          />
        )}
        <span className="shrink-0 w-6 text-[10px] font-mono tabular-nums text-dim text-right">
          {pin.number}
        </span>
        <span
          aria-hidden
          className={[
            "shrink-0 w-2 h-2 rounded-full",
            dotClassForKind(pin.kind),
          ].join(" ")}
        />
        <span className="flex-1 min-w-0 flex items-baseline gap-2">
          <span
            className={[
              "text-xs font-mono",
              pin.kind === "gpio" ? "text-primary" : "text-secondary",
            ].join(" ")}
          >
            {pin.name}
          </span>
          <span className="text-[10px] uppercase tracking-wide text-muted truncate">
            {pin.alt}
          </span>
        </span>
        {chip && (
          <span
            className={[
              "shrink-0 ml-auto flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] font-mono tabular-nums border",
              isOtg
                ? otgChipClasses(chip.value === 1)
                : chip.mode === "output"
                  ? "border-accent/40 bg-accent/10 text-accent"
                  : "border-border-subtle bg-surface/60 text-secondary",
            ].join(" ")}
          >
            <span>{isOtg ? "5V" : chip.mode === "input" ? "IN" : "OUT"}</span>
            <span className="text-primary">{chip.value}</span>
          </span>
        )}
      </button>
    </li>
  );
}

function dotClassForKind(kind: PinKind): string {
  switch (kind) {
    case "gpio":
      return "bg-accent";
    case "power":
      return "bg-success";
    case "ground":
      return "bg-dim";
    case "debug":
    case "i2c":
    case "1wire":
      // No semantic blue in the token set — these utility pins share a single
      // "system / firmware-owned" muted look.
      return "bg-secondary";
    default:
      return "bg-muted";
  }
}

function otgChipClasses(on: boolean): string {
  return on
    ? "border-success/40 bg-success/10 text-success"
    : "border-border-subtle bg-surface/60 text-secondary";
}
