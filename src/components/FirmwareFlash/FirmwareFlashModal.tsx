import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  AlertTriangle,
  Archive,
  Check,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  Cpu,
  Download,
  FileDown,
  HardDriveDownload,
  Loader2,
  Power,
  ShieldCheck,
  Upload,
  X,
  XCircle,
  Zap,
} from "lucide-react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { useFlipperStore } from "../../store/useFlipperStore";
import {
  cancelFirmwareFlash,
  firmwareFetchDirectory,
  firmwareFlash,
  firmwareProviders,
  onFlashProgress,
  type FirmwareCatalog,
  type FirmwareProvider,
  type FlashLevel,
  type FlashProgress,
  type FlashSource,
  type FlashStage,
} from "../../lib/firmware";

interface Props {
  onClose: () => void;
}

interface LogLine {
  id: number;
  ts: number;
  stage: FlashStage;
  level: FlashLevel;
  message: string;
}

type Phase = "idle" | "running" | "done" | "error";

export function FirmwareFlashModal({ onClose }: Props) {
  const deviceInfo = useFlipperStore((s) => s.deviceInfo);
  const isConnected = useFlipperStore((s) => s.isConnected);
  const connectionKind = useFlipperStore((s) => s.connectionKind);
  const setConnected = useFlipperStore((s) => s.setConnected);

  const [providers, setProviders] = useState<FirmwareProvider[]>([]);
  const [providerId, setProviderId] = useState("official");
  const [catalog, setCatalog] = useState<FirmwareCatalog | null>(null);
  const [catalogLoading, setCatalogLoading] = useState(false);
  const [catalogError, setCatalogError] = useState<string | null>(null);
  const [channelId, setChannelId] = useState<string>("");
  const [versionIdx, setVersionIdx] = useState(0);
  /** "online" = a provider/channel/version; "local" = a user-supplied bundle. */
  const [sourceMode, setSourceMode] = useState<"online" | "local">("online");
  const [localFile, setLocalFile] = useState<{ path: string; name: string } | null>(null);

  const [clean, setClean] = useState(true);
  const [showChangelog, setShowChangelog] = useState(false);

  const [phase, setPhase] = useState<Phase>("idle");
  const [cancelPending, setCancelPending] = useState(false);
  const [cancelSettled, setCancelSettled] = useState<"cancelled" | "too_late" | null>(null);
  const [log, setLog] = useState<LogLine[]>([]);
  const [progress, setProgress] = useState<{ stage: FlashStage; pct: number | null }>({
    stage: "download",
    pct: 0,
  });
  const logIdRef = useRef(0);
  const logEndRef = useRef<HTMLDivElement | null>(null);

  const isLocal = sourceMode === "local";
  const running = phase === "running";

  // ── Load providers once ──────────────────────────────────────────────────
  useEffect(() => {
    firmwareProviders()
      .then(setProviders)
      .catch(() => setProviders([]));
  }, []);

  // ── Fetch the selected provider's directory ──────────────────────────────
  // Runs regardless of source mode so flipping back from a local file to an
  // online channel is instant.
  useEffect(() => {
    let cancelled = false;
    setCatalogLoading(true);
    setCatalogError(null);
    setCatalog(null);
    firmwareFetchDirectory(providerId)
      .then((cat) => {
        if (cancelled) return;
        setCatalog(cat);
        const preferred =
          cat.channels.find((c) => c.id === "release") ?? cat.channels[0];
        setChannelId(preferred?.id ?? "");
        setVersionIdx(0);
      })
      .catch((e) => {
        if (cancelled) return;
        setCatalogError(e instanceof Error ? e.message : String(e));
      })
      .finally(() => {
        if (!cancelled) setCatalogLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [providerId]);

  // ── Live progress stream ─────────────────────────────────────────────────
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    onFlashProgress((p: FlashProgress) => {
      setProgress((prev) => ({ stage: p.stage, pct: p.pct ?? prev.pct }));
      if (p.message) {
        setLog((prev) => [
          ...prev,
          {
            id: logIdRef.current++,
            ts: Date.now(),
            stage: p.stage,
            level: p.level,
            message: p.message,
          },
        ]);
      }
    }).then((u) => {
      if (cancelled) u();
      else unlisten = u;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  // ── Auto-scroll the console ──────────────────────────────────────────────
  useEffect(() => {
    logEndRef.current?.scrollIntoView({ block: "end" });
  }, [log]);

  // ── Escape closes when not mid-flash ─────────────────────────────────────
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !running) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose, running]);

  const channel = useMemo(
    () => catalog?.channels.find((c) => c.id === channelId) ?? null,
    [catalog, channelId],
  );
  const version = channel?.versions[versionIdx] ?? null;
  const installed = deviceInfo?.firmware_version ?? null;

  const wrongTransport = isConnected && connectionKind !== "serial";
  const canFlash =
    phase !== "running" &&
    isConnected &&
    connectionKind === "serial" &&
    (isLocal ? !!localFile : !!version);

  // Open the file picker and, if a file is chosen, switch to local mode.
  const chooseLocalFile = useCallback(async () => {
    const picked = await openDialog({
      multiple: false,
      filters: [
        { name: "Firmware update package", extensions: ["tgz", "gz", "tar"] },
        { name: "All files", extensions: ["*"] },
      ],
    });
    const path = Array.isArray(picked) ? picked[0] : picked;
    if (!path) return;
    const name = path.split(/[/\\]/).pop() ?? path;
    setLocalFile({ path, name });
    setSourceMode("local");
  }, []);

  // Leave local mode, back to the selected online provider/channel/version.
  const useOnlineSource = useCallback(() => setSourceMode("online"), []);

  const handleFlash = useCallback(async () => {
    if (!canFlash) return;
    let source: FlashSource;
    if (isLocal) {
      if (!localFile) return;
      source = { kind: "local", local_path: localFile.path };
    } else {
      if (!version) return;
      source = {
        kind: "remote",
        provider_id: providerId,
        channel_id: channelId,
        version: version.version,
        timestamp: version.timestamp,
        selection_token: version.selection_token,
      };
    }
    setPhase("running");
    setCancelPending(false);
    setCancelSettled(null);
    setLog([]);
    setProgress({ stage: "download", pct: 0 });
    try {
      await firmwareFlash(source, { clean });
      setPhase("done");
      // The device rebooted into its own updater — the RPC link is gone.
      setConnected(null);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      setLog((prev) => [
        ...prev,
        {
          id: logIdRef.current++,
          ts: Date.now(),
          stage: "error",
          level: "error",
          message: msg,
        },
      ]);
      setPhase("error");
    }
  }, [
    canFlash,
    isLocal,
    localFile,
    version,
    providerId,
    channelId,
    clean,
    setConnected,
  ]);

  const handleCancel = useCallback(async () => {
    if (cancelPending || cancelSettled !== null) return;
    setCancelPending(true);
    try {
      const response = await cancelFirmwareFlash();
      setLog((prev) => [
        ...prev,
        {
          id: logIdRef.current++,
          ts: Date.now(),
          stage: response.status === "too_late" ? "install" : progress.stage,
          level: response.status === "too_late" ? "warn" : "info",
          message: response.message,
        },
      ]);
      if (response.status === "cancelled" || response.status === "too_late") {
        setCancelSettled(response.status);
      }
    } catch (error) {
      setLog((prev) => [
        ...prev,
        {
          id: logIdRef.current++,
          ts: Date.now(),
          stage: "error",
          level: "error",
          message: error instanceof Error ? error.message : String(error),
        },
      ]);
    } finally {
      setCancelPending(false);
    }
  }, [cancelPending, cancelSettled, progress.stage]);

  return (
    <div
      className="fixed inset-0 z-40 flex items-center justify-center bg-black/60"
      onClick={() => !running && onClose()}
    >
      <div
        className="bg-panel border border-border-subtle rounded-lg shadow-2xl w-full max-w-3xl mx-4 flex flex-col max-h-[86vh]"
        onClick={(e) => e.stopPropagation()}
      >
        {/* ── Header: source selector + Flash ─────────────────────────────── */}
        <header className="flex items-center gap-2 px-4 py-2.5 border-b border-border-subtle">
          <HardDriveDownload size={15} className="text-accent shrink-0" />
          <h3 className="text-sm font-semibold text-primary shrink-0">
            Firmware Flash
          </h3>

          <div className="flex items-center gap-1.5 flex-1 min-w-0 justify-end">
            {isLocal ? (
              <span
                className="inline-flex items-center gap-1 max-w-[16rem] px-2 py-1 text-[11px] font-mono text-primary bg-surface border border-border-subtle rounded"
                title={localFile?.path ?? ""}
              >
                <FileDown size={12} className="text-accent shrink-0" />
                <span className="truncate">{localFile?.name ?? "no file"}</span>
                <button
                  onClick={useOnlineSource}
                  disabled={running}
                  title="Use an online source instead"
                  className="ml-0.5 text-muted hover:text-primary disabled:opacity-40"
                >
                  <X size={11} />
                </button>
              </span>
            ) : (
              <>
                <Select
                  value={providerId}
                  disabled={running}
                  onChange={setProviderId}
                  title="Firmware source"
                >
                  {providers.map((p) => (
                    <option key={p.id} value={p.id}>
                      {p.name}
                    </option>
                  ))}
                </Select>
                <Select
                  value={channelId}
                  disabled={running || !catalog}
                  onChange={(v) => {
                    setChannelId(v);
                    setVersionIdx(0);
                  }}
                  title="Channel"
                >
                  {catalog?.channels.map((c) => (
                    <option key={c.id} value={c.id}>
                      {c.title}
                    </option>
                  ))}
                </Select>
                <Select
                  value={String(versionIdx)}
                  disabled={running || !channel}
                  onChange={(v) => setVersionIdx(Number(v))}
                  title="Version"
                >
                  {channel?.versions.map((v, i) => (
                    <option key={`${v.version}-${i}`} value={String(i)}>
                      {v.version}
                    </option>
                  ))}
                </Select>
              </>
            )}

            <button
              onClick={() => void chooseLocalFile()}
              disabled={running}
              title="Flash a firmware update package (.tgz) from your computer"
              className="inline-flex items-center gap-1 px-2 py-1 text-[11px] text-secondary hover:text-primary border border-border-subtle rounded hover:bg-elevated disabled:opacity-40 disabled:cursor-not-allowed shrink-0"
            >
              <Upload size={12} />
              {isLocal ? "Change…" : "From file…"}
            </button>

            <button
              onClick={() => void handleFlash()}
              disabled={!canFlash}
              title={
                !isConnected
                  ? "Connect a Flipper to flash"
                  : wrongTransport
                    ? "Flashing requires a USB connection"
                    : "Flash the selected firmware"
              }
              className="inline-flex items-center gap-1.5 px-3 py-1 text-xs font-semibold rounded bg-accent text-black hover:bg-accent-hover disabled:opacity-40 disabled:cursor-not-allowed transition-colors"
            >
              {running ? (
                <Loader2 size={13} className="animate-spin" />
              ) : (
                <Zap size={13} />
              )}
              {running ? "Flashing" : "Flash"}
            </button>
          </div>

          <button
            onClick={onClose}
            disabled={running}
            className="p-1 text-secondary hover:text-primary rounded hover:bg-elevated disabled:opacity-40"
            title="Close"
          >
            <X size={14} />
          </button>
        </header>

        {/* ── Body: cockpit (left) + console (right) ──────────────────────── */}
        <div className="flex-1 min-h-0 flex">
          {/* Left: target + options */}
          <aside className="w-56 shrink-0 border-r border-border-subtle p-3 flex flex-col gap-3 overflow-y-auto">
            <InfoBlock>
              <InfoRow label="Target" value="f7" />
              <InfoRow
                label="Build"
                value={isLocal ? "local" : version?.version ?? "—"}
                mono
              />
              <InfoRow
                label="Date"
                value={
                  isLocal || !version?.timestamp
                    ? "—"
                    : new Date(version.timestamp * 1000).toLocaleDateString()
                }
              />
              <InfoRow
                label="Channel"
                value={isLocal ? "—" : channel?.title ?? "—"}
              />
            </InfoBlock>

            <div className="flex items-center gap-2 px-2 py-1.5 rounded bg-surface/50 border border-border-subtle text-[11px]">
              <Cpu size={12} className="text-muted shrink-0" />
              <span className="text-muted">Installed</span>
              <span className="ml-auto font-mono text-secondary truncate" title={installed ?? ""}>
                {installed ?? "—"}
              </span>
            </div>

            <div className="flex flex-col gap-1.5">
              <CheckRow
                label="Clean update dir"
                checked={clean}
                disabled={running}
                onToggle={() => setClean((v) => !v)}
              />
              <div className="flex items-start gap-1.5 px-1 text-[10px] leading-relaxed text-dim">
                <ShieldCheck size={11} className="mt-0.5 shrink-0 text-success" />
                {isLocal
                  ? "Local bundles are treated as untrusted and validated before upload."
                  : "Online bundles are re-resolved and checksum-verified by the backend."}
              </div>
            </div>

            {!isLocal && version?.changelog && (
              <div className="mt-auto">
                <button
                  onClick={() => setShowChangelog((s) => !s)}
                  className="inline-flex items-center gap-1 text-[11px] text-secondary hover:text-primary"
                >
                  {showChangelog ? (
                    <ChevronDown size={12} />
                  ) : (
                    <ChevronRight size={12} />
                  )}
                  Changelog
                </button>
                {showChangelog && (
                  <pre className="mt-1.5 max-h-40 overflow-y-auto whitespace-pre-wrap break-words text-[10px] leading-relaxed text-dim font-mono bg-surface/40 border border-border-subtle rounded p-2">
                    {version.changelog}
                  </pre>
                )}
              </div>
            )}
          </aside>

          {/* Right: live console */}
          <div className="flex-1 min-w-0 flex flex-col bg-app/40">
            <div className="px-3 py-1.5 border-b border-border-subtle text-[10px] uppercase tracking-wide text-muted flex items-center gap-1.5">
              <Zap size={11} className="text-accent" /> Flash log
            </div>
            <div className="flex-1 min-h-[260px] overflow-y-auto px-3 py-2 font-mono text-[11px] leading-relaxed">
              {log.length === 0 ? (
                <div className="h-full flex flex-col items-center justify-center gap-2 text-dim text-center px-4">
                  <HardDriveDownload size={26} className="opacity-40" />
                  <p>
                    {isLocal
                      ? `Ready to flash ${localFile?.name ?? "your file"}.`
                      : catalogLoading
                        ? "Loading available firmware…"
                        : catalogError
                          ? "Could not load firmware list."
                          : "Pick a version, or load your own with From file…"}
                  </p>
                  {catalogError && (
                    <p className="text-[10px] text-danger max-w-xs break-words">
                      {catalogError}
                    </p>
                  )}
                  <p className="text-[10px] text-dim/80 max-w-xs">
                    The bundle is uploaded to your Flipper, then the device
                    reboots into its own updater to apply it.
                  </p>
                </div>
              ) : (
                <>
                  {log.map((l) => (
                    <LogRow key={l.id} line={l} />
                  ))}
                  <div ref={logEndRef} />
                </>
              )}
            </div>
          </div>
        </div>

        {/* ── Footer: progress + actions ──────────────────────────────────── */}
        <footer className="px-4 py-2.5 border-t border-border-subtle bg-surface/40 flex flex-col gap-2">
          {wrongTransport && phase === "idle" && (
            <div className="flex items-center gap-1.5 text-[11px] text-warning">
              <AlertTriangle size={12} />
              Flashing requires a USB connection — reconnect over USB.
            </div>
          )}
          <div className="flex items-center gap-3">
            <StageBadge phase={phase} stage={progress.stage} />
            <div className="flex-1 h-2 bg-elevated rounded-full overflow-hidden">
              <div
                className={`h-full rounded-full transition-[width] duration-200 ${
                  phase === "error"
                    ? "bg-danger"
                    : phase === "done"
                      ? "bg-success"
                      : "bg-accent"
                }`}
                style={{ width: `${progressWidth(phase, progress.pct)}%` }}
              />
            </div>
            <span className="text-[11px] tabular-nums text-secondary w-10 text-right">
              {progress.pct != null && running ? `${progress.pct}%` : ""}
            </span>

            {running ? (
              <button
                onClick={() => void handleCancel()}
                disabled={cancelPending || cancelSettled !== null}
                className="px-3 py-1.5 text-xs rounded bg-surface text-secondary hover:text-primary hover:bg-elevated disabled:opacity-50 disabled:cursor-not-allowed"
              >
                {cancelPending
                  ? "Cancelling…"
                  : cancelSettled === "too_late"
                    ? "Too late to cancel"
                    : cancelSettled === "cancelled"
                      ? "Cancel requested"
                      : "Cancel"}
              </button>
            ) : (
              <button
                onClick={onClose}
                className="px-3 py-1.5 text-xs rounded bg-surface text-secondary hover:text-primary hover:bg-elevated"
              >
                {phase === "done" ? "Done" : "Close"}
              </button>
            )}
          </div>
        </footer>
      </div>
    </div>
  );
}

// ── Footer status helpers ────────────────────────────────────────────────────

function progressWidth(phase: Phase, pct: number | null): number {
  if (phase === "done") return 100;
  if (phase === "idle") return 0;
  return Math.max(0, Math.min(100, pct ?? 0));
}

const STAGE_LABEL: Record<FlashStage, string> = {
  download: "Downloading",
  verify: "Verifying",
  prepare: "Unpacking",
  upload: "Uploading",
  install: "Staging update",
  reboot: "Rebooting",
  done: "Updater started",
  error: "Failed",
};

function StageBadge({ phase, stage }: { phase: Phase; stage: FlashStage }) {
  if (phase === "idle") {
    return <span className="text-[11px] text-dim w-32 shrink-0">Idle</span>;
  }
  if (phase === "done") {
    return (
      <span className="inline-flex items-center gap-1 text-[11px] text-success w-32 shrink-0">
        <CheckCircle2 size={13} /> Update staged
      </span>
    );
  }
  if (phase === "error") {
    return (
      <span className="inline-flex items-center gap-1 text-[11px] text-danger w-32 shrink-0">
        <XCircle size={13} /> {STAGE_LABEL[stage] === "Failed" ? "Failed" : "Aborted"}
      </span>
    );
  }
  return (
    <span className="inline-flex items-center gap-1 text-[11px] text-secondary w-32 shrink-0">
      <Loader2 size={12} className="animate-spin text-accent" />
      {STAGE_LABEL[stage]}
    </span>
  );
}

// ── Log row ──────────────────────────────────────────────────────────────────

const LEVEL_COLOR: Record<FlashLevel, string> = {
  info: "text-secondary",
  ok: "text-success",
  warn: "text-warning",
  error: "text-danger",
};

function stageGlyph(line: LogLine) {
  const cls = LEVEL_COLOR[line.level];
  const size = 12;
  if (line.level === "ok") return <Check size={size} className={cls} />;
  if (line.level === "warn") return <AlertTriangle size={size} className={cls} />;
  if (line.level === "error") return <XCircle size={size} className={cls} />;
  switch (line.stage) {
    case "download":
      return <Download size={size} className={cls} />;
    case "verify":
      return <ShieldCheck size={size} className={cls} />;
    case "prepare":
      return <Archive size={size} className={cls} />;
    case "upload":
      return <Upload size={size} className={cls} />;
    case "install":
      return <Zap size={size} className={cls} />;
    case "reboot":
      return <Power size={size} className={cls} />;
    default:
      return <Check size={size} className={cls} />;
  }
}

function LogRow({ line }: { line: LogLine }) {
  const t = new Date(line.ts);
  const hh = String(t.getHours()).padStart(2, "0");
  const mm = String(t.getMinutes()).padStart(2, "0");
  const ss = String(t.getSeconds()).padStart(2, "0");
  return (
    <div className="flex items-start gap-2 py-0.5">
      <span className="text-dim shrink-0">
        {hh}:{mm}:{ss}
      </span>
      <span className="shrink-0 mt-[1px]">{stageGlyph(line)}</span>
      <span className={`${LEVEL_COLOR[line.level]} break-words min-w-0`}>
        {line.message}
      </span>
    </div>
  );
}

// ── Small UI atoms ───────────────────────────────────────────────────────────

function Select({
  value,
  onChange,
  disabled,
  title,
  children,
}: {
  value: string;
  onChange: (v: string) => void;
  disabled?: boolean;
  title?: string;
  children: React.ReactNode;
}) {
  return (
    <select
      value={value}
      disabled={disabled}
      title={title}
      onChange={(e) => onChange(e.target.value)}
      className="px-2 py-1 text-[11px] text-primary bg-surface border border-border-subtle rounded outline-none focus:border-accent/40 disabled:opacity-40 disabled:cursor-not-allowed max-w-[10rem] truncate"
    >
      {children}
    </select>
  );
}

function InfoBlock({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-1 px-2.5 py-2 rounded bg-surface/50 border border-border-subtle">
      {children}
    </div>
  );
}

function InfoRow({
  label,
  value,
  mono,
}: {
  label: string;
  value: string;
  mono?: boolean;
}) {
  return (
    <div className="flex items-center justify-between gap-2 text-[11px]">
      <span className="text-muted">{label}</span>
      <span
        className={`text-primary truncate ${mono ? "font-mono" : ""}`}
        title={value}
      >
        {value}
      </span>
    </div>
  );
}

function CheckRow({
  label,
  checked,
  disabled,
  onToggle,
}: {
  label: string;
  checked: boolean;
  disabled?: boolean;
  onToggle: () => void;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      disabled={disabled}
      onClick={onToggle}
      className="flex items-center gap-2 text-[11px] text-secondary hover:text-primary disabled:opacity-40 disabled:cursor-not-allowed"
    >
      <span
        className={`inline-flex h-3.5 w-3.5 items-center justify-center rounded border ${
          checked
            ? "bg-accent border-accent text-black"
            : "bg-surface border-border-subtle"
        }`}
      >
        {checked && <Check size={10} />}
      </span>
      {label}
    </button>
  );
}
