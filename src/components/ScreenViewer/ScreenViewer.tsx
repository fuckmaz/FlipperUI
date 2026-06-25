import { useEffect, useRef, useState, useCallback } from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import { save } from "@tauri-apps/plugin-dialog";
import { writeFile } from "@tauri-apps/plugin-fs";
import { Monitor, ZoomIn, ZoomOut, Camera, Circle, Square, ArrowUp, ArrowDown, ArrowLeft, ArrowRight, Check, Undo2, Maximize, Minimize } from "lucide-react";
import { GIFEncoder, applyPalette } from "gifenc";
import { screenStreamStart, screenStreamStop, sendInputEvent } from "../../lib/tauri";
import { base64ToUint8Array } from "../../lib/encoding";
import { loadSettings, subscribeSettings, type AppSettings } from "../../lib/settings";
import { Spinner } from "../ui/Spinner";

const SCREEN_W = 128;
const SCREEN_H = 64;
const SCREEN_ASPECT = SCREEN_W / SCREEN_H;
const SCREEN_FRAME_BORDER_PX = 5;
const FULLSCREEN_PADDING_Y = 16;
const FULLSCREEN_HEADER_ESTIMATE = 45;
const FULLSCREEN_DPAD_ESTIMATE = 150;
const WINDOW_CHROME_ESTIMATE = 30;

// Hard cap on recording length. 60 s * ~30 fps ≈ 1800 frames; at 128×64 RGBA
// that's ~58 MB in memory — above this the UI starts to feel it, and GIFs this
// long are rarely useful anyway.
const MAX_RECORD_MS = 60_000;

// Two-color palette matching the on-screen render: dark segments and the
// amber backlight. Keeping the GIF at 2 colors makes it tiny and sharp.
const GIF_PALETTE: [number, number, number][] = [
  [0x00, 0x00, 0x00],
  [0xff, 0x83, 0x00],
];

// InputKey enum values from Flipper protobuf
const KEY_UP = 0, KEY_DOWN = 1, KEY_RIGHT = 2, KEY_LEFT = 3, KEY_OK = 4, KEY_BACK = 5;
// We only ever emit SHORT or LONG; the backend brackets each one with the
// PRESS / RELEASE the firmware needs (see `write_input` in commands/gui.rs).
const INPUT_SHORT = 2;
const INPUT_LONG = 3;

// Long-press threshold: Flipper firmware fires LONG after ~300 ms of hold.
// We use a slightly higher value so a quick tap can't accidentally trigger it
// across browser keyboard-repeat startup latency.
const LONG_PRESS_MS = 350;

// Cap for the input wait-list. Mashing keys or holding auto-repeat past this
// depth drops the extra events instead of letting a multi-second backlog build
// up — the device would still be replaying old presses long after you let go.
const MAX_PENDING_INPUTS = 8;

function formatElapsed(ms: number): string {
  const total = Math.floor(ms / 1000);
  const m = Math.floor(total / 60);
  const s = total % 60;
  return `${m}:${s.toString().padStart(2, "0")}`;
}

const SCALES = [1, 2, 3, 4, 5] as const;
const DEFAULT_SCALE_INDEX = 3;

function formatScale(scale: number): string {
  return `${scale}x`;
}

function DpadBtn({ icon, onShort, onLong, ariaLabel, label }: { icon: React.ReactNode; onShort: () => void; onLong: () => void; ariaLabel: string; label?: string }) {
  const longTimer = useRef<number | null>(null);
  const longSent = useRef(false);

  // Mouse press lifecycle, mirroring the keyboard handler: a quick click sends
  // SHORT; holding past LONG_PRESS_MS sends LONG instead. The end listener is on
  // `window` (not the button) so releasing after dragging the cursor off the
  // button still resolves the press and can't leak the long timer.
  const begin = (e: React.MouseEvent) => {
    e.preventDefault();
    longSent.current = false;
    longTimer.current = window.setTimeout(() => {
      longSent.current = true;
      longTimer.current = null;
      onLong();
    }, LONG_PRESS_MS);

    const end = () => {
      window.removeEventListener("mouseup", end);
      const wasLong = longSent.current;
      if (longTimer.current != null) {
        window.clearTimeout(longTimer.current);
        longTimer.current = null;
      }
      if (!wasLong) onShort();
    };
    window.addEventListener("mouseup", end);
  };

  return (
    <button
      onMouseDown={begin}
      aria-label={ariaLabel}
      className="flex flex-col items-center justify-center gap-0.5 w-9 h-9 rounded-md border border-border-subtle bg-surface hover:bg-elevated hover:border-flipper/40 active:bg-flipper/30 active:border-flipper text-secondary hover:text-primary transition-colors"
      title={ariaLabel}
    >
      {icon}
      {label && <span className="text-[9px] text-dim leading-none">{label}</span>}
    </button>
  );
}

export function ScreenViewer() {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const headerRef = useRef<HTMLDivElement>(null);
  const screenAreaRef = useRef<HTMLDivElement>(null);
  const dpadRef = useRef<HTMLDivElement>(null);
  const [connected, setConnected] = useState(false);
  const [scaleIndex, setScaleIndex] = useState(DEFAULT_SCALE_INDEX);
  const scale = SCALES[scaleIndex];
  const [error, setError] = useState<string | null>(null);
  const [isRecording, setIsRecording] = useState(false);
  const [recordElapsed, setRecordElapsed] = useState(0);
  const [fullscreenScreenSize, setFullscreenScreenSize] = useState<{
    width: number;
    height: number;
  } | null>(null);

  // Ref (not state) so the frame listener sees updates without re-subscribing —
  // re-subscribing would reset the stream. Frames collected here are encoded
  // into a GIF when the user stops recording.
  const recordingRef = useRef<{ frames: { rgba: Uint8Array; ts: number }[]; startedAt: number } | null>(null);

  // Read on demand inside save handlers so the latest persisted dirs are used
  // even if the user changes them while the viewer is open.
  const settingsRef = useRef<AppSettings | null>(null);
  useEffect(() => {
    loadSettings().then((s) => { settingsRef.current = s; }).catch(() => {});
    return subscribeSettings((s) => { settingsRef.current = s; });
  }, []);

  const [isFullscreen, setIsFullscreen] = useState(false);

  const toggleFullscreen = useCallback(() => {
    setIsFullscreen((prev) => !prev);
  }, []);

  const joinDir = (dir: string | null | undefined, filename: string) =>
    dir ? `${dir.replace(/[\\/]+$/, "")}/${filename}` : filename;

  // Lock the OS window's minimum height while fullscreen is active so the
  // full-width stream usually fits. The canvas sizing still has its own
  // aspect-ratio guard below, so smaller displays fall back to contain sizing
  // instead of distorting.
  useEffect(() => {
    if (!isFullscreen) return;
    const win = getCurrentWindow();

    const apply = () => {
      const innerW = window.innerWidth;
      const reserved =
        (headerRef.current?.offsetHeight ?? FULLSCREEN_HEADER_ESTIMATE) +
        (dpadRef.current?.offsetHeight ?? (connected ? FULLSCREEN_DPAD_ESTIMATE : 0)) +
        FULLSCREEN_PADDING_Y +
        WINDOW_CHROME_ESTIMATE;
      const minHeight = Math.ceil(innerW / SCREEN_ASPECT) + reserved;
      void win.setMinSize(new LogicalSize(400, minHeight)).catch(() => {});
      if (window.innerHeight < minHeight) {
        void win.setSize(new LogicalSize(innerW, minHeight)).catch(() => {});
      }
    };

    apply();
    window.addEventListener("resize", apply);
    return () => {
      window.removeEventListener("resize", apply);
      // Clear the constraint so the user can shrink the window again after
      // exiting fullscreen.
      void win.setMinSize(null).catch(() => {});
    };
  }, [isFullscreen, connected]);

  useEffect(() => {
    if (!isFullscreen) {
      setFullscreenScreenSize(null);
      return;
    }

    const area = screenAreaRef.current;
    if (!area) return;

    const apply = () => {
      const frameChrome = SCREEN_FRAME_BORDER_PX * 2;
      const availableWidth = area.clientWidth - frameChrome;
      const availableHeight = area.clientHeight - frameChrome;
      if (availableWidth <= 0 || availableHeight <= 0) return;

      const nextWidth = Math.floor(
        Math.min(availableWidth, availableHeight * SCREEN_ASPECT),
      );
      const nextHeight = Math.floor(nextWidth / SCREEN_ASPECT);
      setFullscreenScreenSize((prev) =>
        prev?.width === nextWidth && prev?.height === nextHeight
          ? prev
          : { width: nextWidth, height: nextHeight },
      );
    };

    apply();
    const observer = new ResizeObserver(apply);
    observer.observe(area);
    return () => observer.disconnect();
  }, [isFullscreen, connected]);

  const zoomIn = useCallback(() => {
    setScaleIndex((i) => Math.min(SCALES.length - 1, i + 1));
  }, []);
  const zoomOut = useCallback(() => {
    setScaleIndex((i) => Math.max(0, i - 1));
  }, []);

  const saveScreenshot = useCallback(async () => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const dataUrl = canvas.toDataURL("image/png");
    const b64 = dataUrl.replace(/^data:image\/png;base64,/, "");
    const bytes = base64ToUint8Array(b64);

    const path = await save({
      defaultPath: joinDir(settingsRef.current?.screenStream.screenshotDir, "flipper-screen.png"),
      filters: [{ name: "PNG", extensions: ["png"] }],
    });
    if (path) await writeFile(path, bytes);
  }, []);

  const startRecording = useCallback(() => {
    recordingRef.current = { frames: [], startedAt: performance.now() };
    setRecordElapsed(0);
    setIsRecording(true);
  }, []);

  const stopRecording = useCallback(async () => {
    const rec = recordingRef.current;
    recordingRef.current = null;
    setIsRecording(false);
    if (!rec || rec.frames.length < 2) return;

    const gif = GIFEncoder();
    const frames = rec.frames;
    for (let i = 0; i < frames.length; i++) {
      const f = frames[i];
      const next = frames[i + 1];
      // GIF frame delays are in centiseconds; most viewers clamp anything
      // under ~2cs back up to 10cs, so don't bother going lower.
      const delayMs = next ? Math.max(20, Math.round(next.ts - f.ts)) : 100;
      const indexed = applyPalette(f.rgba, GIF_PALETTE);
      gif.writeFrame(indexed, SCREEN_W, SCREEN_H, { palette: GIF_PALETTE, delay: delayMs });
    }
    gif.finish();
    const bytes = gif.bytes();

    const path = await save({
      defaultPath: joinDir(settingsRef.current?.screenStream.gifDir, "flipper-recording.gif"),
      filters: [{ name: "GIF", extensions: ["gif"] }],
    });
    if (path) await writeFile(path, bytes);
  }, []);

  // Tick the elapsed timer while recording; also auto-stop at MAX_RECORD_MS so
  // we never exceed the memory cap enforced in the frame listener.
  useEffect(() => {
    if (!isRecording) return;
    const id = window.setInterval(() => {
      const rec = recordingRef.current;
      if (!rec) return;
      const elapsed = performance.now() - rec.startedAt;
      setRecordElapsed(elapsed);
      if (elapsed >= MAX_RECORD_MS) stopRecording();
    }, 250);
    return () => window.clearInterval(id);
  }, [isRecording, stopRecording]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let mounted = true;

    (async () => {
      try {
        // Listen for frames before starting the stream
        unlisten = await listen<string>("screen-frame", (event) => {
          if (!mounted) return;
          const canvas = canvasRef.current;
          if (!canvas) return;

          const ctx = canvas.getContext("2d");
          if (!ctx) return;

          // Decode base64 RGBA
          const bytes = base64ToUint8Array(event.payload);
          const imageData = ctx.createImageData(SCREEN_W, SCREEN_H);
          imageData.data.set(bytes);
          ctx.putImageData(imageData, 0, 0);

          // While recording, snapshot this frame. We copy bytes because the
          // decoded buffer is reused per event and would otherwise mutate.
          const rec = recordingRef.current;
          if (rec) {
            const now = performance.now();
            if (now - rec.startedAt >= MAX_RECORD_MS) {
              // Auto-stop at cap — handled below by stopRecording watcher.
              return;
            }
            rec.frames.push({ rgba: new Uint8Array(bytes), ts: now });
          }
        });

        await screenStreamStart();
        if (mounted) setConnected(true);
      } catch (err) {
        if (mounted) setError(String(err));
      }
    })();

    return () => {
      mounted = false;
      unlisten?.();
      // Discard any in-progress recording on unmount — we won't have the UI
      // around to prompt for a save path.
      recordingRef.current = null;
      screenStreamStop().catch(() => {});
      setConnected(false);
    };
  }, []);

  // Serial wait-list for input events. Each call chains onto the previous so
  // only one `sendInputEvent` is in flight at a time — rapid clicks or held
  // keys won't flood the backend channel and starve the screen-stream reader
  // (which used to break BLE framing under input bursts).
  const inputChainRef = useRef<Promise<void>>(Promise.resolve());
  const pendingInputsRef = useRef(0);

  const enqueueInput = useCallback((key: number, inputType: number) => {
    if (pendingInputsRef.current >= MAX_PENDING_INPUTS) return;
    pendingInputsRef.current += 1;
    inputChainRef.current = inputChainRef.current
      .then(() => sendInputEvent(key, inputType))
      .catch(() => {})
      .finally(() => {
        pendingInputsRef.current -= 1;
      });
  }, []);

  const press = useCallback((key: number) => {
    enqueueInput(key, INPUT_SHORT);
  }, [enqueueInput]);

  const longPress = useCallback((key: number) => {
    enqueueInput(key, INPUT_LONG);
  }, [enqueueInput]);

  // Global keyboard shortcuts while the viewer is open, without requiring focus.
  // Skip if the user is typing in an input (e.g. the CLI) so text entry wins.
  //
  // Press lifecycle: keydown arms a LONG_PRESS_MS timer. If the key is released
  // before it fires it's a SHORT; if the timer fires first it's a LONG. Either
  // way exactly one event is emitted, and the backend brackets it with the
  // PRESS / RELEASE the firmware needs. Browser auto-repeat is ignored — a held
  // key is one press, not a stream of taps.
  useEffect(() => {
    if (!connected) return;
    const keyMap: Record<string, number> = {
      ArrowUp: KEY_UP, ArrowDown: KEY_DOWN,
      ArrowLeft: KEY_LEFT, ArrowRight: KEY_RIGHT,
      Enter: KEY_OK, Backspace: KEY_BACK,
    };
    type HeldKey = { fkey: number; longTimer: number | null; longSent: boolean };
    const held = new Map<string, HeldKey>();

    // Cancel every armed timer without emitting anything — used on unmount/blur.
    // Nothing is left "held" on the device because each press resolves to a
    // self-contained SHORT/LONG (the backend supplies its own RELEASE), so an
    // interrupted press simply does nothing rather than sticking a key down.
    const cancelAll = () => {
      for (const [, entry] of held) {
        if (entry.longTimer != null) window.clearTimeout(entry.longTimer);
      }
      held.clear();
    };

    const isTextTarget = (t: EventTarget | null) => {
      const el = t as HTMLElement | null;
      if (!el) return false;
      const tag = el.tagName;
      return tag === "INPUT" || tag === "TEXTAREA" || el.isContentEditable;
    };

    const onKeyDown = (e: KeyboardEvent) => {
      if (isTextTarget(e.target)) return;
      const fkey = keyMap[e.key];
      if (fkey === undefined) return;
      e.preventDefault();
      if (e.repeat) return; // hold = one press, not many
      if (held.has(e.key)) return;

      const entry: HeldKey = { fkey, longTimer: null, longSent: false };
      entry.longTimer = window.setTimeout(() => {
        entry.longSent = true;
        entry.longTimer = null;
        enqueueInput(fkey, INPUT_LONG);
      }, LONG_PRESS_MS);
      held.set(e.key, entry);
    };

    const onKeyUp = (e: KeyboardEvent) => {
      const entry = held.get(e.key);
      if (!entry) return;
      held.delete(e.key);
      // Released before the long timer fired → it was a SHORT tap. If the long
      // already fired, it sent its own event and there's nothing left to do.
      if (entry.longTimer != null) {
        window.clearTimeout(entry.longTimer);
        enqueueInput(entry.fkey, INPUT_SHORT);
      }
    };

    // Alt-tabbing mid-hold means we'd never see keyup; drop the pending press.
    window.addEventListener("keydown", onKeyDown);
    window.addEventListener("keyup", onKeyUp);
    window.addEventListener("blur", cancelAll);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("keyup", onKeyUp);
      window.removeEventListener("blur", cancelAll);
      cancelAll();
    };
  }, [connected, enqueueInput]);

  // Fullscreen shortcuts: F toggles, Escape exits
  useEffect(() => {
    const isTextTarget = (t: EventTarget | null) => {
      const el = t as HTMLElement | null;
      if (!el) return false;
      const tag = el.tagName;
      return tag === "INPUT" || tag === "TEXTAREA" || el.isContentEditable;
    };
    const onKeyDown = (e: KeyboardEvent) => {
      if (isTextTarget(e.target)) return;
      if (!e.repeat && (e.key === "f" || e.key === "F")) {
        e.preventDefault();
        toggleFullscreen();
      } else if (e.key === "Escape" && isFullscreen) {
        e.preventDefault();
        setIsFullscreen(false);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [toggleFullscreen, isFullscreen]);

  const width = SCREEN_W * scale;
  const height = SCREEN_H * scale;

  return (
    <div
      className={
        isFullscreen
          ? "fixed inset-0 z-50 flex flex-col bg-app overflow-hidden"
          : "flex-1 min-h-0 flex flex-col bg-app overflow-hidden"
      }
    >
      {/* Header */}
      <div ref={headerRef} className="flex items-center justify-between px-3 py-2 border-b border-border-subtle bg-panel/50 shrink-0">
        <div className="flex items-center gap-2.5 min-w-0">
          <Monitor size={14} className="text-accent shrink-0" />
          <span className="text-xs text-primary font-medium">Screen</span>
          {!connected && !error && (
            <span className="text-[11px] text-dim">connecting…</span>
          )}
          {isRecording && (
            <span
              className="flex items-center gap-1.5 px-2 py-0.5 rounded-full bg-danger/15 border border-danger/30 text-danger text-[11px] font-mono tabular-nums"
              role="status"
              aria-live="polite"
            >
              <span className="relative flex h-2 w-2">
                <span className="animate-ping absolute inline-flex h-full w-full rounded-full bg-danger opacity-75" />
                <span className="relative inline-flex h-2 w-2 rounded-full bg-danger" />
              </span>
              <span>REC {formatElapsed(recordElapsed)}</span>
            </span>
          )}
        </div>
        <div className="flex items-center gap-1.5">
          <button
            onClick={toggleFullscreen}
            aria-label={isFullscreen ? "Exit fullscreen" : "Enter fullscreen"}
            title={isFullscreen ? "Exit fullscreen (F / Esc)" : "Fullscreen (F)"}
            className="p-1 text-muted hover:text-primary rounded transition-colors"
          >
            {isFullscreen ? <Minimize size={14} /> : <Maximize size={14} />}
          </button>

          {/* Zoom group */}
          <div className="flex items-center gap-0.5 px-1 py-0.5 rounded border border-border-subtle bg-surface/40">
            <button
              onClick={zoomOut}
              disabled={scaleIndex === 0}
              aria-label="Zoom out"
              className="p-0.5 text-muted hover:text-primary rounded transition-colors disabled:opacity-30 disabled:cursor-not-allowed"
              title="Zoom out"
            >
              <ZoomOut size={13} />
            </button>
            <span className="text-[10px] text-dim w-9 text-center tabular-nums">
              {formatScale(scale)}
            </span>
            <button
              onClick={zoomIn}
              disabled={scaleIndex === SCALES.length - 1}
              aria-label="Zoom in"
              className="p-0.5 text-muted hover:text-primary rounded transition-colors disabled:opacity-30 disabled:cursor-not-allowed"
              title="Zoom in"
            >
              <ZoomIn size={13} />
            </button>
          </div>

          <button
            onClick={saveScreenshot}
            disabled={!connected}
            aria-label="Save screenshot"
            className="p-1 text-muted hover:text-primary rounded transition-colors disabled:opacity-30 disabled:cursor-not-allowed"
            title="Save screenshot (PNG)"
          >
            <Camera size={14} />
          </button>

          {/* Divider */}
          <div className="w-px h-5 bg-border-subtle mx-0.5" />

          {/* Prominent GIF record button */}
          <button
            onClick={isRecording ? stopRecording : startRecording}
            disabled={!connected}
            aria-label={isRecording ? "Stop recording" : "Record GIF"}
            title={isRecording ? "Stop recording" : "Record GIF"}
            className={`flex items-center gap-1.5 px-2.5 py-1 rounded-full text-[11px] font-medium border transition-colors disabled:opacity-30 disabled:cursor-not-allowed ${
              isRecording
                ? "bg-danger/20 border-danger/50 text-danger hover:bg-danger/30"
                : "bg-surface/60 border-border-subtle text-secondary hover:bg-danger/10 hover:border-danger/40 hover:text-danger"
            }`}
          >
            {isRecording ? (
              <>
                <Square size={11} fill="currentColor" />
                <span>Stop</span>
              </>
            ) : (
              <>
                <Circle size={11} fill="currentColor" className="text-danger" />
                <span>Record GIF</span>
              </>
            )}
          </button>
        </div>
      </div>

      {/* Canvas */}
      <div
        ref={screenAreaRef}
        className={`flex-1 min-h-0 bg-app flex items-center justify-center overflow-auto ${
          isFullscreen ? "p-2" : "p-6"
        }`}
      >
        {error ? (
          <div className="text-xs text-danger px-4 py-8">{error}</div>
        ) : (
          <div
            style={
              isFullscreen
                ? {
                    width: fullscreenScreenSize?.width ?? width,
                    height: fullscreenScreenSize?.height ?? height,
                  }
                : { width, height }
            }
            className={`relative rounded-md border-[5px] border-[#FF8300] overflow-hidden transition-shadow ${
              isRecording
                ? "shadow-[0_0_0_2px_rgba(239,68,68,0.55),0_0_28px_4px_rgba(239,68,68,0.25)]"
                : ""
            }`}
          >
            {!connected && (
              <div className="absolute inset-0 flex items-center justify-center">
                <Spinner size={20} />
              </div>
            )}
            <canvas
              ref={canvasRef}
              width={SCREEN_W}
              height={SCREEN_H}
              style={{
                width: "100%",
                height: "100%",
                imageRendering: "pixelated",
                opacity: connected ? 1 : 0.15,
              }}
              className="block"
            />
          </div>
        )}
      </div>

      {/* D-pad */}
      {connected && (
        <div ref={dpadRef} className="flex items-center justify-center px-3 py-4 gap-5 border-t border-border-subtle bg-panel/30 shrink-0">
          {/* Directional pad */}
          <div className="grid grid-cols-3 grid-rows-3 gap-1">
            <div />
            <DpadBtn icon={<ArrowUp size={14} />} onShort={() => press(KEY_UP)} onLong={() => longPress(KEY_UP)} ariaLabel="Up" />
            <div />
            <DpadBtn icon={<ArrowLeft size={14} />} onShort={() => press(KEY_LEFT)} onLong={() => longPress(KEY_LEFT)} ariaLabel="Left" />
            <DpadBtn icon={<Check size={14} />} onShort={() => press(KEY_OK)} onLong={() => longPress(KEY_OK)} ariaLabel="OK" />
            <DpadBtn icon={<ArrowRight size={14} />} onShort={() => press(KEY_RIGHT)} onLong={() => longPress(KEY_RIGHT)} ariaLabel="Right" />
            <div />
            <DpadBtn icon={<ArrowDown size={14} />} onShort={() => press(KEY_DOWN)} onLong={() => longPress(KEY_DOWN)} ariaLabel="Down" />
            <div />
          </div>
          {/* Back */}
          <DpadBtn icon={<Undo2 size={14} />} onShort={() => press(KEY_BACK)} onLong={() => longPress(KEY_BACK)} ariaLabel="Back" label="Back" />
        </div>
      )}
    </div>
  );
}
