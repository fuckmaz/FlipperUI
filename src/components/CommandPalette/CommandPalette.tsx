import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Bluetooth,
  FolderTree,
  Home,
  Info,
  Monitor,
  Power,
  Search,
  Settings as SettingsIcon,
  Terminal,
  Usb,
  Zap,
  type LucideIcon,
} from "lucide-react";
import { useFlipperStore, type ActiveView } from "../../store/useFlipperStore";
import { disconnect, reboot } from "../../lib/tauri";
import { commandErrorMessage } from "../../lib/commandError";
import { resolveFeatureAvailability } from "../../lib/capabilities";

type CommandKind = "nav" | "action";

interface Command {
  id: string;
  kind: CommandKind;
  label: string;
  hint?: string;
  Icon: LucideIcon;
  /** Synonyms folded into the fuzzy-match haystack. */
  keywords?: string[];
  /** When set, the command is dimmed and not selectable. */
  disabledReason?: string;
  run: () => void | Promise<void>;
}

export function CommandPalette() {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [activeIdx, setActiveIdx] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);

  const setActiveView = useFlipperStore((s) => s.setActiveView);
  const setConnected = useFlipperStore((s) => s.setConnected);
  const setError = useFlipperStore((s) => s.setError);
  const isConnected = useFlipperStore((s) => s.isConnected);
  const connectionKind = useFlipperStore((s) => s.connectionKind);
  const deviceInfo = useFlipperStore((s) => s.deviceInfo);
  const cliConnected = useFlipperStore((s) => s.cliConnected);
  const transferProgress = useFlipperStore((s) => s.transferProgress);
  const fileBrowserDeleting = useFlipperStore((s) => s.fileBrowserDeleting);

  // Cmd/Ctrl+K toggles open. Esc closes. Open from anywhere — including inside
  // input fields — so the user always has an escape hatch to navigate.
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setOpen((v) => !v);
        return;
      }
      if (e.key === "Escape" && open) {
        e.preventDefault();
        setOpen(false);
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [open]);

  // Reset query + selection each time the palette opens, and focus the input.
  useEffect(() => {
    if (open) {
      setQuery("");
      setActiveIdx(0);
      // Focus after the dialog has actually mounted.
      requestAnimationFrame(() => inputRef.current?.focus());
    }
  }, [open]);

  const close = useCallback(() => setOpen(false), []);

  const navigate = useCallback(
    (view: ActiveView) => {
      setActiveView(view);
      close();
    },
    [setActiveView, close],
  );

  const commands = useMemo<Command[]>(() => {
    const busyReason = transferProgress !== null
      ? "A file transfer currently owns device work"
      : fileBrowserDeleting
        ? "A file deletion currently owns device work"
        : false;
    const lockedReason = cliConnected
      ? "Terminal mode currently owns the connection"
      : false;
    const cliAvailability = resolveFeatureAvailability(deviceInfo?.capabilities.cli, {
      busy: busyReason,
    });
    const rpcAvailability = resolveFeatureAvailability(deviceInfo?.capabilities.rpc, {
      busy: busyReason,
      locked: lockedReason,
    });
    const screenAvailability = resolveFeatureAvailability(
      deviceInfo?.capabilities.screen_stream,
      { busy: busyReason, locked: lockedReason },
    );
    const storageAvailability = resolveFeatureAvailability(
      deviceInfo?.capabilities.storage,
      { locked: lockedReason },
    );
    const firmwareAvailability = resolveFeatureAvailability(
      deviceInfo?.capabilities.firmware_update,
      { busy: busyReason, locked: lockedReason },
    );
    const navItems: Command[] = [
      { id: "nav:dashboard", kind: "nav", label: "Dashboard", Icon: Home, keywords: ["home", "overview"], run: () => navigate("dashboard") },
      { id: "nav:files", kind: "nav", label: "File Explorer", Icon: FolderTree, keywords: ["browse", "storage", "ext"], disabledReason: !isConnected ? "Connect first" : storageAvailability.available ? undefined : storageAvailability.reason, run: () => navigate("files") },
      { id: "nav:apps", kind: "nav", label: "Apps", Icon: Zap, keywords: ["fap", "plugins"], disabledReason: !isConnected ? "Connect first" : storageAvailability.available ? undefined : storageAvailability.reason, run: () => navigate("apps") },
      { id: "nav:subghz", kind: "nav", label: "Sub-GHz library", Icon: Zap, keywords: ["radio", "rf", "sub"], run: () => navigate("subghz") },
      { id: "nav:infrared", kind: "nav", label: "Infrared library", Icon: Zap, keywords: ["ir", "remote"], run: () => navigate("infrared") },
      { id: "nav:nfc", kind: "nav", label: "NFC library", Icon: Zap, keywords: ["card", "13.56", "mifare"], run: () => navigate("nfc") },
      { id: "nav:rfid", kind: "nav", label: "RFID library", Icon: Zap, keywords: ["lf", "lfrfid", "125khz", "em4100", "prox"], run: () => navigate("rfid") },
      { id: "nav:badusb", kind: "nav", label: "BadUSB library", Icon: Zap, keywords: ["ducky", "keystrokes"], run: () => navigate("badusb") },
      { id: "nav:screen", kind: "nav", label: "Live screen", Icon: Monitor, keywords: ["mirror", "stream"], disabledReason: !isConnected ? "Connect first" : screenAvailability.available ? undefined : screenAvailability.reason, run: () => navigate("screen") },
      { id: "nav:cli", kind: "nav", label: "Terminal", Icon: Terminal, keywords: ["cli", "shell"], disabledReason: !isConnected ? "Connect first" : cliAvailability.available ? undefined : cliAvailability.reason, run: () => navigate("cli") },
      { id: "nav:info", kind: "nav", label: "Device info", Icon: Info, keywords: ["firmware", "battery"], disabledReason: !isConnected ? "Connect first" : undefined, run: () => navigate("info") },
      { id: "nav:settings", kind: "nav", label: "Settings", Icon: SettingsIcon, keywords: ["preferences", "config"], run: () => navigate("settings") },
    ];

    const actionItems: Command[] = [
      {
        id: "act:disconnect",
        kind: "action",
        label: "Disconnect",
        Icon: connectionKind === "ble" ? Bluetooth : Usb,
        keywords: ["unplug", "stop"],
        disabledReason: !isConnected ? "Not connected" : undefined,
        run: async () => {
          close();
          try {
            await disconnect();
            setConnected(null);
          } catch (e) {
            setError(`Disconnect failed: ${commandErrorMessage(e, "disconnect")}`);
          }
        },
      },
      {
        id: "act:reboot",
        kind: "action",
        label: "Reboot device",
        hint: "Restart the Flipper into normal mode",
        Icon: Power,
        keywords: ["restart"],
        disabledReason: !isConnected ? "Connect first" : rpcAvailability.available ? undefined : rpcAvailability.reason,
        run: async () => {
          close();
          try {
            await reboot(0);
          } catch (e) {
            setError(`Reboot failed: ${commandErrorMessage(e, "reboot")}`);
          }
        },
      },
      {
        id: "act:reboot-dfu",
        kind: "action",
        label: "Reboot into DFU",
        hint: "Bootloader mode for firmware flashing",
        Icon: Power,
        keywords: ["bootloader", "flash", "update"],
        disabledReason: !isConnected ? "Connect first" : firmwareAvailability.available ? undefined : firmwareAvailability.reason,
        run: async () => {
          close();
          try {
            await reboot(1);
          } catch (e) {
            setError(`Reboot failed: ${commandErrorMessage(e, "reboot_dfu")}`);
          }
        },
      },
    ];

    return [...navItems, ...actionItems];
  }, [
    isConnected,
    connectionKind,
    deviceInfo,
    cliConnected,
    transferProgress,
    fileBrowserDeleting,
    navigate,
    close,
    setConnected,
    setError,
  ]);

  const filtered = useMemo(() => fuzzyFilter(commands, query), [commands, query]);

  // Clamp the active index when the filtered list changes — otherwise pressing
  // Enter could fire a stale, off-screen item.
  useEffect(() => {
    if (activeIdx >= filtered.length) setActiveIdx(0);
  }, [filtered.length, activeIdx]);

  // Keep the active row visible while arrow-keying through a long list.
  useEffect(() => {
    const el = listRef.current?.querySelector<HTMLElement>(
      `[data-idx="${activeIdx}"]`,
    );
    el?.scrollIntoView({ block: "nearest" });
  }, [activeIdx]);

  if (!open) return null;

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setActiveIdx((i) => moveSelectable(filtered, i, +1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActiveIdx((i) => moveSelectable(filtered, i, -1));
    } else if (e.key === "Enter") {
      e.preventDefault();
      const cmd = filtered[activeIdx];
      if (cmd && !cmd.disabledReason) void cmd.run();
    }
  };

  return (
    <div
      role="dialog"
      aria-modal
      aria-label="Command palette"
      className="fixed inset-0 z-[100] flex items-start justify-center pt-[12vh] bg-app/70 backdrop-blur-sm"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) close();
      }}
    >
      <div className="w-[min(560px,92vw)] max-h-[70vh] flex flex-col bg-panel border border-border-subtle rounded-lg shadow-2xl overflow-hidden">
        <div className="flex items-center gap-2 px-3 py-2 border-b border-border-subtle">
          <Search size={14} className="text-dim" />
          <input
            ref={inputRef}
            value={query}
            onChange={(e) => {
              setQuery(e.target.value);
              setActiveIdx(0);
            }}
            onKeyDown={onKeyDown}
            placeholder="Type a command or jump to…"
            className="flex-1 bg-transparent text-sm text-primary placeholder:text-dim outline-none"
          />
          <kbd className="hidden sm:inline-flex items-center gap-1 px-1.5 py-0.5 text-[10px] text-dim border border-border-subtle rounded">
            Esc
          </kbd>
        </div>

        <div ref={listRef} className="flex-1 min-h-0 overflow-y-auto py-1">
          {filtered.length === 0 ? (
            <div className="px-4 py-6 text-center text-xs text-dim">
              No commands match “{query}”.
            </div>
          ) : (
            filtered.map((cmd, i) => (
              <CommandRow
                key={cmd.id}
                cmd={cmd}
                active={i === activeIdx}
                index={i}
                onHover={() => setActiveIdx(i)}
                onClick={() => {
                  if (cmd.disabledReason) return;
                  void cmd.run();
                }}
              />
            ))
          )}
        </div>

        <div className="shrink-0 flex items-center justify-between gap-3 px-3 py-1.5 border-t border-border-subtle text-[10px] text-dim">
          <div className="flex items-center gap-3">
            <span>
              <kbd className="text-[10px]">↑↓</kbd> navigate
            </span>
            <span>
              <kbd className="text-[10px]">↵</kbd> select
            </span>
          </div>
          <span>
            <kbd className="text-[10px]">⌘K</kbd> to toggle
          </span>
        </div>
      </div>
    </div>
  );
}

function CommandRow({
  cmd,
  active,
  index,
  onHover,
  onClick,
}: {
  cmd: Command;
  active: boolean;
  index: number;
  onHover: () => void;
  onClick: () => void;
}) {
  const { Icon } = cmd;
  const disabled = !!cmd.disabledReason;
  return (
    <div
      data-idx={index}
      role="option"
      aria-selected={active}
      aria-disabled={disabled}
      onMouseMove={onHover}
      onMouseDown={(e) => {
        e.preventDefault();
        onClick();
      }}
      className={`flex items-center gap-3 px-3 py-2 mx-1 rounded cursor-pointer text-sm ${
        disabled
          ? "text-dim cursor-not-allowed"
          : active
            ? "bg-surface text-primary"
            : "text-secondary hover:text-primary"
      }`}
    >
      <Icon size={14} className={disabled ? "text-dim" : "text-accent"} />
      <span className="flex-1 truncate">{cmd.label}</span>
      {cmd.hint && !disabled && (
        <span className="text-[11px] text-dim truncate">{cmd.hint}</span>
      )}
      {disabled && (
        <span className="text-[10px] text-dim italic">{cmd.disabledReason}</span>
      )}
      <span className="text-[10px] uppercase tracking-wide text-dim">
        {cmd.kind === "nav" ? "Go" : "Run"}
      </span>
    </div>
  );
}

// ── Fuzzy match ──────────────────────────────────────────────────────────────

function fuzzyFilter(commands: Command[], query: string): Command[] {
  const q = query.trim().toLowerCase();
  if (!q) return commands;

  const scored = commands
    .map((c) => {
      const haystack = [c.label, ...(c.keywords ?? [])].join(" ").toLowerCase();
      const score = scoreMatch(haystack, q);
      return { c, score };
    })
    .filter((x) => x.score > 0);

  scored.sort((a, b) => b.score - a.score);
  return scored.map((x) => x.c);
}

// Subsequence match with bonuses for word-start hits and contiguous runs.
// Cheap, deterministic, and good enough for ~15 commands.
function scoreMatch(haystack: string, query: string): number {
  let qi = 0;
  let score = 0;
  let streak = 0;
  let prevWasBoundary = true;

  for (let i = 0; i < haystack.length && qi < query.length; i++) {
    const hc = haystack[i];
    const qc = query[qi];
    if (hc === qc) {
      let bonus = 1;
      if (prevWasBoundary) bonus += 3;
      bonus += streak; // contiguous match bonus
      score += bonus;
      streak += 1;
      qi += 1;
    } else {
      streak = 0;
    }
    prevWasBoundary = hc === " " || hc === "-" || hc === "_";
  }

  return qi === query.length ? score : 0;
}

function moveSelectable(items: Command[], from: number, dir: 1 | -1): number {
  if (items.length === 0) return 0;
  let i = from;
  for (let n = 0; n < items.length; n++) {
    i = (i + dir + items.length) % items.length;
    if (!items[i]?.disabledReason) return i;
  }
  // Everything is disabled — fall back to first index so the UI still moves.
  return (from + dir + items.length) % items.length;
}
