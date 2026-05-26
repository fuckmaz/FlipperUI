import { useState } from "react";
import { Eye, EyeOff, Power, Zap, AlertTriangle } from "lucide-react";
import type {
  GpioMode,
  GpioPinName,
  GpioPull,
  HeaderPin,
} from "../../types/gpio";
import { Sparkline } from "./Sparkline";

interface RpcPinDetailProps {
  pin: HeaderPin;
  mode: GpioMode;
  value: 0 | 1;
  pull: GpioPull;
  watching: boolean;
  samples: ReadonlyArray<0 | 1>;
  lastAction: string | null;
  lastActionAt: number | null;
  busy: boolean;
  onSetMode: (mode: GpioMode) => void;
  onSetPull: (pull: GpioPull) => void;
  onToggleWatch: () => void;
  onReadNow: () => void;
  onWriteValue: (value: 0 | 1) => void;
  onPulse: () => void;
}

/** Right-pane detail card for one of the 8 RPC-controllable GPIO pins. */
export function RpcPinDetail({
  pin,
  mode,
  value,
  pull,
  watching,
  samples,
  lastAction,
  lastActionAt,
  busy,
  onSetMode,
  onSetPull,
  onToggleWatch,
  onReadNow,
  onWriteValue,
  onPulse,
}: RpcPinDetailProps) {
  return (
    <div className="flex flex-col gap-4">
      <DetailHeader pin={pin} />

      <Section title="Mode">
        <Segmented
          options={[
            { value: "input", label: "INPUT" },
            { value: "output", label: "OUTPUT" },
          ]}
          value={mode}
          onChange={(m) => onSetMode(m as GpioMode)}
          disabled={busy}
        />
      </Section>

      {mode === "input" ? (
        <InputControls
          pin={pin.name as GpioPinName}
          value={value}
          pull={pull}
          watching={watching}
          samples={samples}
          busy={busy}
          onSetPull={onSetPull}
          onToggleWatch={onToggleWatch}
          onReadNow={onReadNow}
        />
      ) : (
        <OutputControls
          value={value}
          busy={busy}
          onWriteValue={onWriteValue}
          onPulse={onPulse}
        />
      )}

      <Footer text={lastAction} ts={lastActionAt} />
    </div>
  );
}

// ── Sub-sections ────────────────────────────────────────────────────────────

function InputControls({
  pin,
  value,
  pull,
  watching,
  samples,
  busy,
  onSetPull,
  onToggleWatch,
  onReadNow,
}: {
  pin: GpioPinName;
  value: 0 | 1;
  pull: GpioPull;
  watching: boolean;
  samples: ReadonlyArray<0 | 1>;
  busy: boolean;
  onSetPull: (pull: GpioPull) => void;
  onToggleWatch: () => void;
  onReadNow: () => void;
}) {
  // pin is intentionally accepted so callers don't need to thread it; we use
  // it as an aria-hint for screen readers.
  return (
    <>
      <Section title="Pull">
        <Segmented
          options={[
            { value: "no", label: "None" },
            { value: "up", label: "Pull-up" },
            { value: "down", label: "Pull-down" },
          ]}
          value={pull}
          onChange={(p) => onSetPull(p as GpioPull)}
          disabled={busy}
        />
      </Section>

      <Section title="Value">
        <div
          className={[
            "flex items-center gap-3 px-4 py-4 rounded-lg border",
            value === 1
              ? "border-success/40 bg-success/10"
              : "border-border-subtle bg-surface/60",
          ].join(" ")}
          aria-label={`${pin} logic level`}
        >
          <span
            aria-hidden
            className={[
              "w-3 h-3 rounded-full",
              value === 1 ? "bg-success" : "bg-dim",
            ].join(" ")}
          />
          <span className="text-2xl font-semibold tabular-nums text-primary">
            {value === 1 ? "HIGH" : "LOW"}
          </span>
          <span className="ml-auto text-[10px] uppercase tracking-wide text-muted">
            logic
          </span>
        </div>
        <div className="mt-2 text-muted">
          <Sparkline samples={samples} height={28} />
        </div>
      </Section>

      <Section title="Watch">
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={onToggleWatch}
            disabled={busy}
            className={[
              "inline-flex items-center gap-1.5 px-2.5 py-1.5 rounded text-xs border transition-colors disabled:opacity-50 disabled:cursor-not-allowed",
              watching
                ? "border-accent/40 bg-accent/10 text-accent hover:bg-accent/20"
                : "border-border-subtle bg-surface text-secondary hover:text-primary hover:bg-elevated",
            ].join(" ")}
          >
            {watching ? <EyeOff size={12} /> : <Eye size={12} />}
            {watching ? "Stop watching" : "Watch"}
          </button>
          <button
            type="button"
            onClick={onReadNow}
            disabled={busy || watching}
            title={watching ? "Disable Watch to read manually" : "Read once"}
            className="inline-flex items-center gap-1.5 px-2.5 py-1.5 text-xs rounded border border-border-subtle bg-surface text-secondary hover:text-primary hover:bg-elevated disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
          >
            Read now
          </button>
          {watching && (
            <span className="text-[10px] uppercase tracking-wide text-muted ml-1">
              live
            </span>
          )}
        </div>
      </Section>
    </>
  );
}

function OutputControls({
  value,
  busy,
  onWriteValue,
  onPulse,
}: {
  value: 0 | 1;
  busy: boolean;
  onWriteValue: (value: 0 | 1) => void;
  onPulse: () => void;
}) {
  const next: 0 | 1 = value === 1 ? 0 : 1;
  return (
    <Section title="Output">
      <div className="flex flex-col gap-2">
        <button
          type="button"
          onClick={() => onWriteValue(next)}
          disabled={busy}
          className={[
            "w-full flex items-center justify-center gap-3 px-4 py-4 rounded-lg border text-2xl font-semibold tabular-nums transition-colors disabled:opacity-50 disabled:cursor-not-allowed",
            value === 1
              ? "border-success/40 bg-success/10 text-success hover:bg-success/20"
              : "border-border-subtle bg-surface text-secondary hover:bg-elevated hover:text-primary",
          ].join(" ")}
          aria-pressed={value === 1}
        >
          <span
            aria-hidden
            className={[
              "w-3 h-3 rounded-full",
              value === 1 ? "bg-success" : "bg-dim",
            ].join(" ")}
          />
          {value === 1 ? "HIGH" : "LOW"}
          <span className="ml-1 text-[10px] uppercase tracking-wide text-muted">
            click to {value === 1 ? "drive low" : "drive high"}
          </span>
        </button>
        <button
          type="button"
          onClick={onPulse}
          disabled={busy}
          title="Drive HIGH for 100 ms, then LOW"
          className="inline-flex items-center justify-center gap-1.5 px-2.5 py-1.5 text-xs rounded border border-border-subtle bg-surface text-secondary hover:text-primary hover:bg-elevated disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
        >
          <Zap size={12} />
          Pulse 100 ms
        </button>
      </div>
    </Section>
  );
}

// ── Pin 1 — +5V OTG card ────────────────────────────────────────────────────

export function OtgPinDetail({
  pin,
  otg,
  busy,
  lastAction,
  lastActionAt,
  onToggleOtg,
}: {
  pin: HeaderPin;
  otg: boolean;
  busy: boolean;
  lastAction: string | null;
  lastActionAt: number | null;
  onToggleOtg: (on: boolean) => void;
}) {
  return (
    <div className="flex flex-col gap-4">
      <DetailHeader pin={pin} />
      <Section title="+5 V OTG">
        <div className="flex items-center gap-3">
          <button
            type="button"
            onClick={() => onToggleOtg(!otg)}
            disabled={busy}
            aria-pressed={otg}
            className={[
              "relative inline-flex items-center w-12 h-6 rounded-full transition-colors disabled:opacity-50 disabled:cursor-not-allowed",
              otg ? "bg-success/40" : "bg-elevated",
            ].join(" ")}
          >
            <span
              aria-hidden
              className={[
                "absolute top-0.5 w-5 h-5 rounded-full bg-primary transition-transform",
                otg ? "translate-x-6" : "translate-x-0.5",
              ].join(" ")}
            />
          </button>
          <span className="text-sm font-medium text-primary">
            {otg ? "5 V rail on" : "5 V rail off"}
          </span>
          <Power
            size={14}
            className={otg ? "text-success" : "text-dim"}
          />
        </div>
        <div className="mt-3 flex items-start gap-2 px-3 py-2 rounded border border-warning/30 bg-warning/10 text-[11px] text-warning">
          <AlertTriangle size={14} className="shrink-0 mt-0.5" />
          <span>
            Sources up to 0.5 A from the battery. Loads beyond that may
            brown-out the device or shut OTG off automatically.
          </span>
        </div>
      </Section>
      <Footer text={lastAction} ts={lastActionAt} />
    </div>
  );
}

// ── Info-only pins (power, ground, debug, i2c, 1wire) ──────────────────────

export function InfoPinDetail({ pin }: { pin: HeaderPin }) {
  return (
    <div className="flex flex-col gap-4">
      <DetailHeader pin={pin} />
      <Section title="About this pin">
        <p className="text-xs text-secondary leading-relaxed">
          {pin.note ?? "Not controllable from this view."}
        </p>
        <p className="mt-2 text-[11px] text-dim">
          This pin is not exposed through the Flipper's GPIO RPC commands, so
          it can't be read or driven from here.
        </p>
      </Section>
    </div>
  );
}

// ── Shared chrome ────────────────────────────────────────────────────────────

function DetailHeader({ pin }: { pin: HeaderPin }) {
  return (
    <div className="flex items-center gap-3 pb-2 border-b border-border-subtle">
      <span className="inline-flex items-center justify-center w-7 h-7 rounded bg-elevated text-[11px] font-mono tabular-nums text-primary">
        {pin.number}
      </span>
      <div className="flex flex-col min-w-0">
        <span className="text-sm font-mono font-semibold text-primary">
          {pin.name}
        </span>
        <span className="text-[11px] uppercase tracking-wide text-muted">
          {pin.alt}
        </span>
      </div>
    </div>
  );
}

function Section({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <section>
      <h4 className="text-[10px] uppercase tracking-wide text-muted mb-2">
        {title}
      </h4>
      {children}
    </section>
  );
}

interface SegmentedOption {
  value: string;
  label: string;
}

function Segmented({
  options,
  value,
  onChange,
  disabled,
}: {
  options: SegmentedOption[];
  value: string;
  onChange: (next: string) => void;
  disabled?: boolean;
}) {
  return (
    <div
      role="radiogroup"
      className="inline-flex p-0.5 rounded-md border border-border-subtle bg-surface"
    >
      {options.map((opt) => {
        const active = opt.value === value;
        return (
          <button
            key={opt.value}
            type="button"
            role="radio"
            aria-checked={active}
            onClick={() => {
              if (!active) onChange(opt.value);
            }}
            disabled={disabled}
            className={[
              "px-3 py-1.5 text-xs rounded transition-colors disabled:opacity-50 disabled:cursor-not-allowed",
              active
                ? "bg-accent/15 text-accent"
                : "text-secondary hover:text-primary hover:bg-elevated",
            ].join(" ")}
          >
            {opt.label}
          </button>
        );
      })}
    </div>
  );
}

function Footer({ text, ts }: { text: string | null; ts: number | null }) {
  // Tick over time so the relative timestamp stays fresh while the card is
  // mounted. Lightweight — re-render at most once a second.
  const [, force] = useState(0);
  useTicker(ts !== null, () => force((x) => x + 1));
  if (!text || ts == null) return null;
  return (
    <div className="text-[11px] text-dim pt-2 border-t border-border-subtle">
      {text} · {formatTime(ts)}
    </div>
  );
}

// ── Helpers ──────────────────────────────────────────────────────────────────

import { useEffect } from "react";

function useTicker(active: boolean, cb: () => void) {
  useEffect(() => {
    if (!active) return;
    const id = window.setInterval(cb, 1000);
    return () => window.clearInterval(id);
  }, [active, cb]);
}

function formatTime(ts: number): string {
  const d = new Date(ts);
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  const ss = String(d.getSeconds()).padStart(2, "0");
  return `${hh}:${mm}:${ss}`;
}
