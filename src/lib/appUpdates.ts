import { invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import { create } from "zustand";
import {
  loadSettings,
  updateSettings,
  type AppSettings,
} from "./settings";

const DAILY_CHECK_INTERVAL_MS = 24 * 60 * 60 * 1_000;
export const LATEST_RELEASE_URL =
  "https://github.com/fuckmaz/FlipperUI/releases/latest";

export type AppUpdatePhase =
  | "idle"
  | "checking"
  | "up-to-date"
  | "available"
  | "error";

export interface AppUpdateCheckResult {
  currentVersion: string;
  latestVersion: string;
  available: boolean;
  releaseName: string;
  releaseUrl: string;
  notes: string | null;
  publishedAt: string | null;
}

export interface AppUpdateState {
  phase: AppUpdatePhase;
  visible: boolean;
  currentVersion: string | null;
  latestVersion: string | null;
  releaseName: string | null;
  releaseUrl: string | null;
  notes: string | null;
  publishedAt: string | null;
  error: string | null;
}

const INITIAL_STATE: AppUpdateState = {
  phase: "idle",
  visible: false,
  currentVersion: null,
  latestVersion: null,
  releaseName: null,
  releaseUrl: null,
  notes: null,
  publishedAt: null,
  error: null,
};

export const useAppUpdateStore = create<AppUpdateState>(() => INITIAL_STATE);

let activeCheck: Promise<void> | null = null;
let activeCheckIsInteractive = false;

export function shouldCheckForAppUpdates(
  settings: Pick<AppSettings, "updates">,
  now = Date.now(),
): boolean {
  if (settings.updates.checkFrequency === "manual") return false;
  if (settings.updates.checkFrequency === "startup") return true;

  const lastChecked = settings.updates.lastCheckedAt
    ? Date.parse(settings.updates.lastCheckedAt)
    : Number.NaN;
  return (
    !Number.isFinite(lastChecked) ||
    now - lastChecked >= DAILY_CHECK_INTERVAL_MS
  );
}

export async function runAutomaticAppUpdateCheck(): Promise<void> {
  const settings = await loadSettings().catch(() => null);
  if (!settings || !shouldCheckForAppUpdates(settings)) return;
  await checkForAppUpdate(false);
}

/** Hourly foreground tick; only the daily policy participates. */
export async function runPeriodicAppUpdateCheck(): Promise<void> {
  const settings = await loadSettings().catch(() => null);
  if (
    !settings ||
    settings.updates.checkFrequency !== "daily" ||
    !shouldCheckForAppUpdates(settings)
  ) {
    return;
  }
  await checkForAppUpdate(false);
}

/**
 * Ask the Rust backend for GitHub's latest published stable release.
 * Automatic checks only surface a release once per version; manual checks
 * always show a result.
 */
export async function checkForAppUpdate(interactive = true): Promise<void> {
  if (activeCheck) {
    if (interactive) {
      activeCheckIsInteractive = true;
      useAppUpdateStore.setState({ visible: true });
    }
    return activeCheck;
  }

  activeCheckIsInteractive = interactive;
  activeCheck = (async () => {
    useAppUpdateStore.setState({
      phase: "checking",
      visible: interactive,
      error: null,
    });

    try {
      const [result, settings] = await Promise.all([
        invoke<AppUpdateCheckResult>("app_update_check"),
        loadSettings(),
      ]);
      const checkedAt = new Date().toISOString();

      if (result.available) {
        const firstNoticeForVersion =
          settings.updates.lastNotifiedVersion !== result.latestVersion;

        await updateSettings({
          updates: {
            lastCheckedAt: checkedAt,
            lastNotifiedVersion: result.latestVersion,
          },
        }).catch(() => {});

        useAppUpdateStore.setState({
          phase: "available",
          visible: activeCheckIsInteractive || firstNoticeForVersion,
          currentVersion: result.currentVersion,
          latestVersion: result.latestVersion,
          releaseName: result.releaseName,
          releaseUrl: result.releaseUrl,
          notes: result.notes,
          publishedAt: result.publishedAt,
          error: null,
        });
        return;
      }

      await updateSettings({
        updates: { lastCheckedAt: checkedAt },
      }).catch(() => {});
      useAppUpdateStore.setState({
        ...INITIAL_STATE,
        phase: "up-to-date",
        visible: activeCheckIsInteractive,
        currentVersion: result.currentVersion,
        latestVersion: result.latestVersion,
      });
    } catch (error) {
      useAppUpdateStore.setState({
        ...useAppUpdateStore.getState(),
        phase: activeCheckIsInteractive ? "error" : "idle",
        visible: activeCheckIsInteractive,
        error: activeCheckIsInteractive ? friendlyUpdateError(error) : null,
      });
    } finally {
      activeCheck = null;
      activeCheckIsInteractive = false;
    }
  })();

  return activeCheck;
}

/** Open the checked release (or GitHub's stable latest-release redirect). */
export async function openLatestAppRelease(): Promise<void> {
  const url = useAppUpdateStore.getState().releaseUrl ?? LATEST_RELEASE_URL;
  try {
    await openUrl(url);
    dismissAppUpdateNotice();
  } catch (error) {
    useAppUpdateStore.setState({
      phase: "error",
      visible: true,
      error: `Could not open the release page: ${friendlyUpdateError(error)}`,
    });
  }
}

export function dismissAppUpdateNotice(): void {
  useAppUpdateStore.setState({ visible: false });
}

function friendlyUpdateError(error: unknown): string {
  const message = error instanceof Error ? error.message : String(error);
  return message
    .replace(/^Error:\s*/i, "")
    .replace(/^could not check GitHub Releases:\s*/i, "")
    .trim();
}
