import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  AlertTriangle,
  CheckCircle2,
  ExternalLink,
  Loader2,
  RefreshCw,
  Rocket,
  X,
} from "lucide-react";
import {
  checkForAppUpdate,
  dismissAppUpdateNotice,
  openLatestAppRelease,
  runAutomaticAppUpdateCheck,
  runPeriodicAppUpdateCheck,
  useAppUpdateStore,
  type AppUpdatePhase,
} from "../../lib/appUpdates";

const NOTICE_DURATION_MS: Partial<Record<AppUpdatePhase, number>> = {
  available: 10_000,
  "up-to-date": 4_000,
  error: 8_000,
};

export function AppUpdateNotice() {
  const update = useAppUpdateStore();
  const [remainingPercent, setRemainingPercent] = useState(100);

  useEffect(() => {
    const timer = window.setTimeout(() => {
      void runAutomaticAppUpdateCheck();
    }, 1_500);
    const interval = window.setInterval(() => {
      void runPeriodicAppUpdateCheck();
    }, 60 * 60 * 1_000);

    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen("check-app-updates", () => {
      void checkForAppUpdate(true);
    }).then((stopListening) => {
      if (disposed) stopListening();
      else unlisten = stopListening;
    });

    return () => {
      disposed = true;
      unlisten?.();
      window.clearTimeout(timer);
      window.clearInterval(interval);
    };
  }, []);

  useEffect(() => {
    if (!update.visible) return;
    const duration = NOTICE_DURATION_MS[update.phase];
    if (!duration) {
      setRemainingPercent(100);
      return;
    }

    const startedAt = performance.now();
    let frame = 0;
    const tick = (now: number) => {
      const remaining = Math.max(0, 1 - (now - startedAt) / duration);
      setRemainingPercent(remaining * 100);
      if (remaining > 0) {
        frame = window.requestAnimationFrame(tick);
      } else {
        dismissAppUpdateNotice();
      }
    };
    setRemainingPercent(100);
    frame = window.requestAnimationFrame(tick);
    return () => window.cancelAnimationFrame(frame);
  }, [update.phase, update.visible]);

  if (!update.visible) return null;

  const hasCountdown = NOTICE_DURATION_MS[update.phase] !== undefined;

  return (
    <aside
      aria-live="polite"
      aria-label="FlipperUI application update"
      className="fixed z-[70] right-4 bottom-4 w-[min(25rem,calc(100vw-2rem))] rounded-lg border border-border-subtle bg-panel shadow-2xl overflow-hidden"
    >
      <div className="flex items-start gap-3 px-4 py-3">
        <UpdateIcon phase={update.phase} />
        <div className="min-w-0 flex-1">
          <h2 className="text-sm font-medium text-primary">
            {noticeTitle(update.phase, update.latestVersion)}
          </h2>
          <NoticeMessage
            phase={update.phase}
            currentVersion={update.currentVersion}
            latestVersion={update.latestVersion}
            error={update.error}
          />
          <NoticeActions phase={update.phase} />
        </div>
        <button
          type="button"
          onClick={dismissAppUpdateNotice}
          aria-label="Dismiss update notice"
          className="p-1 text-muted hover:text-primary rounded transition-colors"
        >
          <X size={14} />
        </button>
      </div>

      {hasCountdown && (
        <div className="h-1 bg-elevated" aria-hidden="true">
          <div
            className="h-full bg-accent"
            style={{ width: `${remainingPercent}%` }}
          />
        </div>
      )}
    </aside>
  );
}

function NoticeMessage({
  phase,
  currentVersion,
  latestVersion,
  error,
}: {
  phase: AppUpdatePhase;
  currentVersion: string | null;
  latestVersion: string | null;
  error: string | null;
}) {
  if (phase === "available") {
    return (
      <p className="mt-1 text-xs leading-relaxed text-secondary">
        You have v{currentVersion ?? "?"}; v{latestVersion ?? "?"} is ready on
        GitHub Releases.
      </p>
    );
  }
  if (phase === "checking") {
    return <p className="mt-1 text-xs text-secondary">Checking GitHub Releases…</p>;
  }
  if (phase === "up-to-date") {
    return (
      <p className="mt-1 text-xs text-secondary">
        This installation is already on the latest published version.
      </p>
    );
  }
  if (phase === "error") {
    return (
      <p className="mt-1 text-xs leading-relaxed text-danger break-words select-text">
        {error ?? "The update check failed."}
      </p>
    );
  }
  return null;
}

function NoticeActions({ phase }: { phase: AppUpdatePhase }) {
  if (phase === "available") {
    return (
      <button
        type="button"
        onClick={() => void openLatestAppRelease()}
        className="mt-2 inline-flex items-center gap-1.5 px-3 py-1.5 text-xs rounded font-medium bg-accent text-black hover:bg-accent-hover transition-colors"
      >
        <ExternalLink size={12} /> View release
      </button>
    );
  }
  if (phase === "error") {
    return (
      <button
        type="button"
        onClick={() => void checkForAppUpdate(true)}
        className="mt-2 inline-flex items-center gap-1.5 px-3 py-1.5 text-xs rounded bg-surface text-secondary hover:text-primary hover:bg-elevated border border-border-subtle transition-colors"
      >
        <RefreshCw size={12} /> Retry
      </button>
    );
  }
  return null;
}

function UpdateIcon({ phase }: { phase: AppUpdatePhase }) {
  if (phase === "checking") {
    return <Loader2 size={17} className="text-accent animate-spin mt-0.5" />;
  }
  if (phase === "up-to-date") {
    return <CheckCircle2 size={17} className="text-success mt-0.5" />;
  }
  if (phase === "error") {
    return <AlertTriangle size={17} className="text-danger mt-0.5" />;
  }
  return <Rocket size={17} className="text-accent mt-0.5" />;
}

function noticeTitle(
  phase: AppUpdatePhase,
  latestVersion: string | null,
): string {
  switch (phase) {
    case "checking":
      return "Checking for FlipperUI updates";
    case "available":
      return latestVersion
        ? `FlipperUI v${latestVersion} is available`
        : "A FlipperUI update is available";
    case "up-to-date":
      return "FlipperUI is up to date";
    case "error":
      return "Could not check for updates";
    default:
      return "FlipperUI updates";
  }
}
