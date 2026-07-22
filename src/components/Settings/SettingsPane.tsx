import { useEffect, useId, useRef, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import {
  confirm as confirmDialog,
  open as openDialog,
  save as saveDialog,
} from "@tauri-apps/plugin-dialog";
import { readTextFile, writeTextFile } from "@tauri-apps/plugin-fs";
import {
  Wrench,
  LayoutGrid,
  Languages,
  Info,
  MonitorCog,
  Bell,
  Plug,
  X,
  Plus,
  Folder,
  FolderOpen,
  MonitorPlay,
  Palette,
  Filter,
  FolderCog,
  Bug,
  MessageSquare,
  Mail,
  ExternalLink,
  GitFork,
  RefreshCw,
  Download,
  Upload,
  RotateCcw,
  LifeBuoy,
} from "lucide-react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { DiagPanel } from "../DevTools/DiagPanel";
import {
  loadSettings,
  exportSettingsJson,
  importSettingsJson,
  resetSettings,
  subscribeSettings,
  updateSettings,
  type AppSettings,
  type UpdateCheckFrequency,
} from "../../lib/settings";
import { useDirectorySuggestions } from "../../lib/useDirectorySuggestions";
import {
  appIconVariants,
  diagEntries,
  setAppIcon,
  type AppIconVariant,
} from "../../lib/tauri";
import {
  ACCENT_PRESETS,
  FLIPPER_ORANGE,
  applyAccentColor,
  normalizeHex,
} from "../../lib/theme";
import { LibraryExclusionsEditor } from "./LibraryExclusionsEditor";
import {
  checkForAppUpdate,
  openLatestAppRelease,
  useAppUpdateStore,
  type AppUpdateState,
} from "../../lib/appUpdates";
import { useFlipperStore } from "../../store/useFlipperStore";
import { useDeviceTelemetry } from "../../lib/deviceTelemetry";
import { serializeSupportBundle } from "../../lib/supportBundle";

const IS_MACOS =
  typeof navigator !== "undefined" && /Mac/i.test(navigator.platform);

const REPO_URL = "https://github.com/fuckmaz/FlipperUI";
const LATEST_RELEASE_URL = `${REPO_URL}/releases/latest`;

const LANGUAGE_OPTIONS = [{ code: "en", label: "English" }];

export function SettingsPane() {
  const [version, setVersion] = useState<string | null>(null);
  const [settings, setSettings] = useState<AppSettings | null>(null);
  const [diagOpen, setDiagOpen] = useState(false);
  const [iconVariants, setIconVariants] = useState<AppIconVariant[]>([]);
  const [settingsDataBusy, setSettingsDataBusy] = useState<
    "export" | "import" | "reset" | "support" | null
  >(null);
  const [settingsDataStatus, setSettingsDataStatus] = useState<{
    message: string;
    error: boolean;
  } | null>(null);
  const appUpdate = useAppUpdateStore();
  const device = useFlipperStore((state) => state.deviceInfo);
  const connectionKind = useFlipperStore((state) => state.connectionKind);
  const telemetry = useDeviceTelemetry();

  useEffect(() => {
    getVersion().then(setVersion).catch(() => {});
    loadSettings().then(setSettings).catch(() => {});
    appIconVariants().then(setIconVariants).catch(() => {});
    return subscribeSettings(setSettings);
  }, []);

  const onLanguageChange = async (lang: string) => {
    const next = await updateSettings({ language: lang });
    setSettings(next);
  };

  const onUpdateCheckFrequencyChange = async (
    checkFrequency: UpdateCheckFrequency,
  ) => {
    const next = await updateSettings({ updates: { checkFrequency } });
    setSettings(next);
  };

  const handleUpdateAction = () => {
    if (appUpdate.phase === "available") return openLatestAppRelease();
    return checkForAppUpdate(true);
  };

  const onAppsExtraChange = async (extraDirs: string[]) => {
    const next = await updateSettings({ apps: { extraDirs } });
    setSettings(next);
  };

  const onTrayEnabledChange = async (enabled: boolean) => {
    const next = await updateSettings({ tray: { enabled } });
    setSettings(next);
    await invoke("set_tray_enabled", { enabled }).catch(() => {});
    // Re-installing the tray rebuilds it from defaults, so re-apply the
    // monochrome preference any time we just turned the tray back on.
    if (enabled && next.tray.monochromeIcon) {
      await invoke("set_tray_monochrome", { monochrome: true }).catch(() => {});
    }
    // If the tray is turned off we also force the dock icon back on — an app
    // with no tray and no dock is unreachable once the window is hidden.
    if (!enabled && next.tray.hideDockIcon) {
      await invoke("set_dock_visible", { visible: true }).catch(() => {});
    } else if (enabled) {
      await invoke("set_dock_visible", {
        visible: !next.tray.hideDockIcon,
      }).catch(() => {});
    }
  };

  const onHideDockChange = async (hideDockIcon: boolean) => {
    const next = await updateSettings({ tray: { hideDockIcon } });
    setSettings(next);
    await invoke("set_dock_visible", { visible: !hideDockIcon }).catch(() => {});
  };

  const onMonochromeIconChange = async (monochromeIcon: boolean) => {
    const next = await updateSettings({ tray: { monochromeIcon } });
    setSettings(next);
    await invoke("set_tray_monochrome", { monochrome: monochromeIcon }).catch(
      () => {},
    );
  };

  const onLibraryScanNotifChange = async (libraryScansFinished: boolean) => {
    const next = await updateSettings({
      notifications: { libraryScansFinished },
    });
    setSettings(next);
  };

  const onPreScanReviewChange = async (preScanReview: boolean) => {
    const next = await updateSettings({ libraries: { preScanReview } });
    setSettings(next);
  };

  const onDisconnectNotifChange = async (deviceDisconnected: boolean) => {
    const next = await updateSettings({
      notifications: { deviceDisconnected },
    });
    setSettings(next);
  };

  const onAutoReconnectChange = async (autoReconnect: boolean) => {
    const next = await updateSettings({ connection: { autoReconnect } });
    setSettings(next);
  };

  const onSyncClockOnConnectChange = async (syncClockOnConnect: boolean) => {
    const next = await updateSettings({ connection: { syncClockOnConnect } });
    setSettings(next);
  };

  const onInlineActionChange = async (
    action: "rename" | "download" | "delete",
    enabled: boolean,
  ) => {
    const current = settings?.fileBrowser.inlineActions ?? {
      rename: true,
      download: true,
      delete: true,
    };
    const next = await updateSettings({
      fileBrowser: { inlineActions: { ...current, [action]: enabled } },
    });
    setSettings(next);
  };

  const onScreenshotDirChange = async (screenshotDir: string | null) => {
    const next = await updateSettings({ screenStream: { screenshotDir } });
    setSettings(next);
  };

  const onGifDirChange = async (gifDir: string | null) => {
    const next = await updateSettings({ screenStream: { gifDir } });
    setSettings(next);
  };

  // Apply live first (cheap CSS-var write) then persist; on failure we
  // roll the CSS-var change back so the running app matches storage.
  const onAccentChange = async (hex: string) => {
    const normalized = normalizeHex(hex) ?? FLIPPER_ORANGE;
    const previous = settings?.appearance.themeAccent ?? FLIPPER_ORANGE;
    if (previous === normalized) return;
    applyAccentColor(normalized);
    try {
      const next = await updateSettings({
        appearance: { themeAccent: normalized },
      });
      setSettings(next);
    } catch {
      applyAccentColor(previous);
    }
  };

  const onAppIconChange = async (variantId: string) => {
    // Persist first so a crash mid-apply doesn't strand the user with the
    // selection but the wrong actual icon. Then apply live; if the live
    // application fails we revert the persisted value to keep them in sync.
    const previous = settings?.appearance.appIcon ?? "default";
    if (previous === variantId) return;
    const next = await updateSettings({ appearance: { appIcon: variantId } });
    setSettings(next);
    try {
      const applied = await setAppIcon(variantId);
      if (applied !== variantId) {
        // Backend resolved the id to something else (unknown variant fell
        // back to default). Reflect the canonical id in settings.
        const corrected = await updateSettings({
          appearance: { appIcon: applied },
        });
        setSettings(corrected);
      }
    } catch {
      const reverted = await updateSettings({
        appearance: { appIcon: previous },
      });
      setSettings(reverted);
    }
  };

  const applyImportedRuntimeSettings = async (next: AppSettings) => {
    applyAccentColor(next.appearance.themeAccent);
    await setAppIcon(next.appearance.appIcon).catch(() => "default");
    await invoke("set_tray_enabled", { enabled: next.tray.enabled }).catch(
      () => {},
    );
    if (next.tray.enabled) {
      await invoke("set_tray_monochrome", {
        monochrome: next.tray.monochromeIcon,
      }).catch(() => {});
    }
    await invoke("set_dock_visible", {
      visible: !next.tray.enabled || !next.tray.hideDockIcon,
    }).catch(() => {});
  };

  const onExportSettings = async () => {
    setSettingsDataBusy("export");
    setSettingsDataStatus(null);
    try {
      const path = await saveDialog({
        title: "Export FlipperUI settings",
        defaultPath: "flipperui-settings.json",
        filters: [{ name: "JSON", extensions: ["json"] }],
      });
      if (!path) return;
      await writeTextFile(path, await exportSettingsJson());
      setSettingsDataStatus({ message: "Settings exported.", error: false });
    } catch (error) {
      setSettingsDataStatus({
        message: `Export failed: ${errorMessage(error)}`,
        error: true,
      });
    } finally {
      setSettingsDataBusy(null);
    }
  };

  const onImportSettings = async () => {
    setSettingsDataBusy("import");
    setSettingsDataStatus(null);
    try {
      const path = await openDialog({
        title: "Import FlipperUI settings",
        multiple: false,
        directory: false,
        filters: [{ name: "JSON", extensions: ["json"] }],
      });
      if (typeof path !== "string") return;
      const next = await importSettingsJson(await readTextFile(path));
      setSettings(next);
      await applyImportedRuntimeSettings(next);
      setSettingsDataStatus({
        message: "Settings imported. Previous settings were backed up.",
        error: false,
      });
    } catch (error) {
      setSettingsDataStatus({
        message: `Import failed: ${errorMessage(error)}`,
        error: true,
      });
    } finally {
      setSettingsDataBusy(null);
    }
  };

  const onResetSettings = async () => {
    const confirmed = await confirmDialog(
      "Reset all FlipperUI settings to their defaults?",
      { title: "Reset settings", kind: "warning" },
    );
    if (!confirmed) return;

    setSettingsDataBusy("reset");
    setSettingsDataStatus(null);
    try {
      const next = await resetSettings();
      setSettings(next);
      await applyImportedRuntimeSettings(next);
      setSettingsDataStatus({ message: "Settings reset to defaults.", error: false });
    } catch (error) {
      setSettingsDataStatus({
        message: `Reset failed: ${errorMessage(error)}`,
        error: true,
      });
    } finally {
      setSettingsDataBusy(null);
    }
  };

  const onExportSupportBundle = async () => {
    if (!settings) return;
    setSettingsDataBusy("support");
    setSettingsDataStatus(null);
    try {
      const path = await saveDialog({
        title: "Export redacted FlipperUI support bundle",
        defaultPath: `flipperui-support-${new Date().toISOString().slice(0, 10)}.json`,
        filters: [{ name: "JSON", extensions: ["json"] }],
      });
      if (!path) return;
      const diagnostics = await diagEntries().catch(() => []);
      const data = serializeSupportBundle({
        generatedAt: new Date(),
        appVersion: version,
        platform: navigator.platform,
        userAgent: navigator.userAgent,
        settings,
        device,
        connectionKind,
        telemetry,
        diagnostics,
      });
      await writeTextFile(path, data);
      setSettingsDataStatus({
        message: "Redacted support bundle exported.",
        error: false,
      });
    } catch (error) {
      setSettingsDataStatus({
        message: `Support export failed: ${errorMessage(error)}`,
        error: true,
      });
    } finally {
      setSettingsDataBusy(null);
    }
  };

  return (
    <div className="flex-1 min-h-0 overflow-y-auto">
      <div className="max-w-2xl mx-auto px-6 py-6 flex flex-col gap-4">
        <header className="flex items-baseline justify-between">
          <h1 className="text-base font-medium text-primary">Settings</h1>
          <span className="text-xs text-dim">
            {version ? `FlipperUI v${version}` : ""}
          </span>
        </header>

        <Section icon={<Info size={13} />} title="About">
          <div className="flex items-center gap-3">
            <img
              src="/flipperui-icon.png"
              alt="FlipperUI"
              width={48}
              height={48}
              className="rounded-lg shadow"
            />
            <div className="flex flex-col text-xs">
              <span className="text-primary font-medium">FlipperUI</span>
              <span className="text-secondary">A Flipper Zero Manager and qFlipper replacement, focused on file browsing and organized libraries for SubGHz, Infrared, NFC and everything else.</span>
              <span className="text-dim italic mt-0.5">in love -maz</span>
              <div className="flex flex-wrap items-center gap-x-3 gap-y-1 mt-1">
                <button
                  onClick={() => openUrl("mailto:maz@postcatz.com")}
                  className="flex items-center gap-1 text-[10px] text-dim hover:text-secondary w-fit transition-colors"
                >
                  <Mail size={9} />
                  send me a mail :)
                </button>
                <button
                  onClick={() => openUrl(LATEST_RELEASE_URL)}
                  title="Open the latest FlipperUI release on GitHub"
                  className="flex items-center gap-1 text-[10px] text-dim hover:text-secondary w-fit transition-colors"
                >
                  <ExternalLink size={9} />
                  Latest release
                </button>
              </div>
            </div>
          </div>
        </Section>

        <Section icon={<RefreshCw size={13} />} title="Application Updates">
          <Row
            label="Check for updates"
            hint="Checks the latest published stable release on GitHub. When a newer version is available, FlipperUI shows a notification that opens the release page in your browser."
          >
            <select
              value={settings?.updates.checkFrequency ?? "daily"}
              disabled={!settings}
              onChange={(event) =>
                void onUpdateCheckFrequencyChange(
                  event.target.value as UpdateCheckFrequency,
                )
              }
              className="bg-surface border border-border-subtle rounded px-2 py-1 text-xs text-primary focus:outline-none focus:border-accent disabled:opacity-50"
              aria-label="Automatic update check frequency"
            >
              <option value="daily">Once a day</option>
              <option value="startup">Every launch</option>
              <option value="manual">Manual only</option>
            </select>
          </Row>
          <div className="flex items-center justify-between gap-4 rounded border border-border-subtle bg-surface/40 px-3 py-2">
            <div className="min-w-0 flex flex-col">
              <span className="text-xs text-primary">
                {appUpdateStatus(appUpdate, settings)}
              </span>
              {settings?.updates.lastCheckedAt && (
                <span className="text-[10px] text-dim mt-0.5">
                  Last checked {formatUpdateCheckTime(settings.updates.lastCheckedAt)}
                </span>
              )}
            </div>
            <button
              type="button"
              onClick={() => void handleUpdateAction()}
              disabled={appUpdate.phase === "checking"}
              className="inline-flex items-center gap-1.5 shrink-0 px-3 py-1.5 text-xs rounded bg-surface text-secondary hover:text-primary hover:bg-elevated border border-border-subtle transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
            >
              <RefreshCw
                size={12}
                className={appUpdate.phase === "checking" ? "animate-spin" : ""}
              />
              {appUpdateActionLabel(appUpdate)}
            </button>
          </div>
        </Section>

        <Section icon={<Languages size={13} />} title="General">
          <Row label="Language" hint="More languages will arrive with i18n.">
            <select
              value={settings?.language ?? "en"}
              onChange={(e) => onLanguageChange(e.target.value)}
              disabled={!settings}
              className="bg-surface border border-border-subtle rounded px-2 py-1 text-xs text-primary focus:outline-none focus:border-accent disabled:opacity-50"
            >
              {LANGUAGE_OPTIONS.map((o) => (
                <option key={o.code} value={o.code}>
                  {o.label}
                </option>
              ))}
            </select>
          </Row>
        </Section>

        <Section icon={<Download size={13} />} title="Settings Data">
          <p className="text-[11px] text-dim">
            Export a portable, versioned backup or restore one. Imports are
            validated before changes are applied and preserve the previous
            settings as an internal rollback copy.
          </p>
          <div className="flex flex-wrap gap-2">
            <SettingsDataButton
              icon={<Download size={12} />}
              label="Export"
              disabled={!settings || settingsDataBusy !== null}
              busy={settingsDataBusy === "export"}
              onClick={() => void onExportSettings()}
            />
            <SettingsDataButton
              icon={<Upload size={12} />}
              label="Import"
              disabled={settingsDataBusy !== null}
              busy={settingsDataBusy === "import"}
              onClick={() => void onImportSettings()}
            />
            <SettingsDataButton
              icon={<RotateCcw size={12} />}
              label="Reset"
              disabled={!settings || settingsDataBusy !== null}
              busy={settingsDataBusy === "reset"}
              danger
              onClick={() => void onResetSettings()}
            />
            <SettingsDataButton
              icon={<LifeBuoy size={12} />}
              label="Support bundle"
              disabled={!settings || settingsDataBusy !== null}
              busy={settingsDataBusy === "support"}
              onClick={() => void onExportSupportBundle()}
            />
          </div>
          <p className="text-[10px] text-dim">
            Support bundles are capped and omit UIDs, ports, paths, secrets,
            diagnostic details, and file payloads.
          </p>
          {settingsDataStatus && (
            <p
              role={settingsDataStatus.error ? "alert" : "status"}
              className={`text-[11px] ${
                settingsDataStatus.error ? "text-danger" : "text-secondary"
              }`}
            >
              {settingsDataStatus.message}
            </p>
          )}
        </Section>

        <Section icon={<Palette size={13} />} title="Appearance">
          <Row
            label="App icon"
            hint={
              IS_MACOS
                ? "Pick the icon used in the Dock and switcher. Changes apply immediately."
                : "Pick the icon used in the taskbar and Start menu. Changes apply immediately."
            }
          >
            {/* Empty slot — the chooser sits below as a full-width grid. */}
            <span className="text-[11px] text-dim">
              {iconVariants.length} {iconVariants.length === 1 ? "option" : "options"}
            </span>
          </Row>
          <AppIconChooser
            variants={iconVariants}
            selected={settings?.appearance.appIcon ?? "default"}
            disabled={!settings || iconVariants.length === 0}
            onChange={onAppIconChange}
          />
          <div className="h-px bg-border-subtle" />
          <Row
            label="Theme accent"
            hint="Tint applied to highlights, focus rings, and active states. The FlipperUI brand orange in the splash and app header stays as-is."
          >
            <span />
          </Row>
          <AccentColorChooser
            value={settings?.appearance.themeAccent ?? FLIPPER_ORANGE}
            disabled={!settings}
            onChange={onAccentChange}
          />
        </Section>

        <Section icon={<MonitorCog size={13} />} title="System">
          <Row
            label="Show tray icon"
            hint="Show the FlipperUI icon in the system tray / menubar. Left-click toggles the window; right-click opens Show/Hide/Quit."
          >
            <Toggle
              checked={settings?.tray.enabled ?? true}
              disabled={!settings}
              onChange={onTrayEnabledChange}
              ariaLabel="Show tray icon"
            />
          </Row>
          <Row
            label="Monochrome tray icon"
            hint={
              IS_MACOS
                ? "Use a flat glyph that adopts the menubar's foreground color (light/dark mode aware)."
                : "Use a flat monochrome glyph instead of the full-color icon."
            }
          >
            <Toggle
              checked={settings?.tray.monochromeIcon ?? false}
              disabled={!settings || !settings.tray.enabled}
              onChange={onMonochromeIconChange}
              ariaLabel="Monochrome tray icon"
            />
          </Row>
          {IS_MACOS && (
            <Row
              label="Hide Dock icon"
              hint={
                settings?.tray.enabled
                  ? "Run as a menubar-only app. The tray icon remains the way to reach the window."
                  : "Enable the tray icon first — otherwise the app would be unreachable with the window hidden."
              }
            >
              <Toggle
                checked={settings?.tray.hideDockIcon ?? false}
                disabled={!settings || !settings.tray.enabled}
                onChange={onHideDockChange}
                ariaLabel="Hide Dock icon"
              />
            </Row>
          )}
        </Section>

        <Section icon={<Plug size={13} />} title="Connection">
          <Row
            label="Auto-connect & auto-reconnect"
            hint="When on, FlipperUI automatically connects to a Flipper as soon as it shows up (USB port detected, or last-paired BLE peripheral) and reconnects after an unexpected drop. Off by default — click Connect manually."
          >
            <Toggle
              checked={settings?.connection.autoReconnect ?? false}
              disabled={!settings}
              onChange={onAutoReconnectChange}
              ariaLabel="Auto-connect and auto-reconnect"
            />
          </Row>
          <Row
            label="Sync clock on connect"
            hint="Set the Flipper RTC from this computer's local date and time after each successful USB or BLE connection."
          >
            <Toggle
              checked={settings?.connection.syncClockOnConnect ?? true}
              disabled={!settings}
              onChange={onSyncClockOnConnectChange}
              ariaLabel="Sync clock on connect"
            />
          </Row>
        </Section>

        <Section icon={<FolderCog size={13} />} title="File Browser">
          <Row
            label="Inline action icons"
            hint="Choose which action icons appear on hover for each file row. All actions are always available via right-click."
          >
            <span />
          </Row>
          <Row label="Rename">
            <Toggle
              checked={settings?.fileBrowser.inlineActions.rename ?? true}
              disabled={!settings}
              onChange={(v) => onInlineActionChange("rename", v)}
              ariaLabel="Show rename icon inline"
            />
          </Row>
          <Row label="Download">
            <Toggle
              checked={settings?.fileBrowser.inlineActions.download ?? true}
              disabled={!settings}
              onChange={(v) => onInlineActionChange("download", v)}
              ariaLabel="Show download icon inline"
            />
          </Row>
          <Row label="Delete">
            <Toggle
              checked={settings?.fileBrowser.inlineActions.delete ?? true}
              disabled={!settings}
              onChange={(v) => onInlineActionChange("delete", v)}
              ariaLabel="Show delete icon inline"
            />
          </Row>
        </Section>

        <Section icon={<Bell size={13} />} title="Notifications">
          <Row
            label="Library scan finished"
            hint="Show a desktop notification each time a library scan (Sub-GHz / Infrared / NFC / RFID / BadUSB / Apps) finishes. The first notification will prompt for OS-level permission."
          >
            <Toggle
              checked={settings?.notifications.libraryScansFinished ?? true}
              disabled={!settings}
              onChange={onLibraryScanNotifChange}
              ariaLabel="Library scan finished notification"
            />
          </Row>
          <Row
            label="Device disconnected"
            hint="Show a desktop notification when the Flipper drops unexpectedly. Manual disconnects via the toolbar never notify."
          >
            <Toggle
              checked={settings?.notifications.deviceDisconnected ?? true}
              disabled={!settings}
              onChange={onDisconnectNotifChange}
              ariaLabel="Device disconnected notification"
            />
          </Row>
        </Section>

        <Section icon={<MonitorPlay size={13} />} title="Screen Stream">
          <Row
            label="Screenshot folder"
            hint="Default folder for `Save screenshot`. The save dialog still appears so you can rename or pick a different location each time."
          >
            <DirectoryPicker
              value={settings?.screenStream.screenshotDir ?? null}
              disabled={!settings}
              onChange={onScreenshotDirChange}
              ariaLabel="Choose default screenshot folder"
            />
          </Row>
          <Row
            label="GIF recording folder"
            hint="Default folder for the GIF recorder's save dialog."
          >
            <DirectoryPicker
              value={settings?.screenStream.gifDir ?? null}
              disabled={!settings}
              onChange={onGifDirChange}
              ariaLabel="Choose default GIF folder"
            />
          </Row>
        </Section>

        <Section icon={<Filter size={13} />} title="Library Exclusions">
          <Row
            label="Pre-scan review"
            hint="Before each library scan, surface directories with 254+ entries or files larger than 1 MiB so you can exclude them. Excluded folders are saved here. Doesn't apply to the Apps library."
          >
            <Toggle
              checked={settings?.libraries.preScanReview ?? true}
              disabled={!settings}
              onChange={onPreScanReviewChange}
              ariaLabel="Pre-scan review of heavy directories"
            />
          </Row>
          <LibraryExclusionsEditor
            settings={settings}
            disabled={!settings}
            onChange={setSettings}
          />
        </Section>

        <Section icon={<LayoutGrid size={13} />} title="Apps">
          <AbsoluteDirListEditor
            heading="Additional app directories"
            description="Extra paths to scan for .fap files, in addition to /ext/apps. Must start with /ext, /int, or /any."
            placeholder="/ext/apps_data"
            disabled={!settings}
            value={settings?.apps.extraDirs ?? []}
            reserved={["/ext/apps"]}
            onChange={onAppsExtraChange}
          />
        </Section>

        <Section icon={<Wrench size={13} />} title="Developer">
          <button
            onClick={() => setDiagOpen(true)}
            className="w-full flex items-center gap-2 px-3 py-2 text-xs text-secondary hover:text-primary hover:bg-surface/60 rounded border border-border-subtle transition-colors text-left"
          >
            <Wrench size={12} />
            <span className="flex-1">Developer diagnostics</span>
            <span className="text-dim">Open →</span>
          </button>
        </Section>

        <SettingsFooter version={version} />
      </div>

      {diagOpen && <DiagPanel onClose={() => setDiagOpen(false)} />}
    </div>
  );
}

function appUpdateStatus(
  update: AppUpdateState,
  settings: AppSettings | null,
): string {
  switch (update.phase) {
    case "checking":
      return "Checking GitHub Releases…";
    case "available":
      return update.latestVersion
        ? `FlipperUI v${update.latestVersion} is available`
        : "An update is available";
    case "up-to-date":
      return "FlipperUI is up to date";
    case "error":
      return update.error ? `Update check failed: ${update.error}` : "Update check failed";
    default:
      return settings?.updates.lastCheckedAt
        ? "No newer version found"
        : "Updates have not been checked yet";
  }
}

function appUpdateActionLabel(update: AppUpdateState): string {
  switch (update.phase) {
    case "checking":
      return "Checking…";
    case "available":
      return "View release";
    case "error":
      return "Retry";
    default:
      return "Check now";
  }
}

function formatUpdateCheckTime(value: string): string {
  const date = new Date(value);
  return Number.isNaN(date.getTime())
    ? "previously"
    : new Intl.DateTimeFormat(undefined, {
        dateStyle: "medium",
        timeStyle: "short",
      }).format(date);
}

function SettingsFooter({ version }: { version: string | null }) {
  const openIssue = (kind: "bug" | "feedback") => {
    const meta = [
      version ? `App version: ${version}` : "",
      `Platform: ${navigator.platform}`,
    ]
      .filter(Boolean)
      .join("\n");

    if (kind === "bug") {
      const body = `**Describe the bug**\nA clear and concise description of what happened.\n\n**Steps to reproduce**\n1. …\n\n**Expected behavior**\n…\n\n---\n${meta}`;
      openUrl(
        `${REPO_URL}/issues/new?labels=bug&title=&body=${encodeURIComponent(body)}`,
      );
    } else {
      const body = `**Feedback**\n\n\n---\n${meta}`;
      openUrl(
        `${REPO_URL}/issues/new?labels=feedback&title=&body=${encodeURIComponent(body)}`,
      );
    }
  };

  return (
    <div className="flex items-center justify-center gap-4 py-3 text-[11px] text-dim">
      <button
        onClick={() => openIssue("bug")}
        className="flex items-center gap-1.5 hover:text-secondary transition-colors"
      >
        <Bug size={11} />
        Report a bug
      </button>
      <span className="text-border-subtle">·</span>
      <button
        onClick={() => openIssue("feedback")}
        className="flex items-center gap-1.5 hover:text-secondary transition-colors"
      >
        <MessageSquare size={11} />
        Send feedback
      </button>
      <span className="text-border-subtle">·</span>
      <button
        onClick={() => openUrl(REPO_URL)}
        title="Open the FlipperUI GitHub repository"
        className="flex items-center gap-1.5 hover:text-secondary transition-colors"
      >
        <GitFork size={11} />
        GitHub repository
      </button>
    </div>
  );
}

function SettingsDataButton({
  icon,
  label,
  disabled,
  busy,
  danger = false,
  onClick,
}: {
  icon: React.ReactNode;
  label: string;
  disabled: boolean;
  busy: boolean;
  danger?: boolean;
  onClick: () => void;
}) {
  const busyLabel =
    label === "Reset" ? "Resetting…" : label === "Import" ? "Importing…" : "Exporting…";
  return (
    <button
      type="button"
      disabled={disabled}
      onClick={onClick}
      className={`inline-flex items-center gap-1.5 rounded border border-border-subtle px-3 py-1.5 text-xs transition-colors disabled:cursor-not-allowed disabled:opacity-40 ${
        danger
          ? "text-danger hover:bg-danger/10"
          : "text-secondary hover:bg-surface/60 hover:text-primary"
      }`}
    >
      {busy ? <RefreshCw size={12} className="animate-spin" /> : icon}
      {busy ? busyLabel : label}
    </button>
  );
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function Section({
  icon,
  title,
  children,
}: {
  icon: React.ReactNode;
  title: string;
  children: React.ReactNode;
}) {
  return (
    <section className="bg-panel border border-border-subtle rounded-lg overflow-hidden">
      <header className="flex items-center gap-2 px-3 py-2 border-b border-border-subtle text-xs text-secondary">
        <span className="text-muted">{icon}</span>
        <span className="font-medium text-primary">{title}</span>
      </header>
      <div className="p-3 flex flex-col gap-3">{children}</div>
    </section>
  );
}

function Row({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="flex items-start justify-between gap-4">
      <div className="flex flex-col">
        <span className="text-xs text-primary">{label}</span>
        {hint && <span className="text-[11px] text-dim mt-0.5">{hint}</span>}
      </div>
      <div className="shrink-0">{children}</div>
    </div>
  );
}

function Toggle({
  checked,
  disabled,
  onChange,
  ariaLabel,
}: {
  checked: boolean;
  disabled?: boolean;
  onChange: (next: boolean) => void;
  ariaLabel: string;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={ariaLabel}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      className={`relative inline-flex h-5 w-9 shrink-0 items-center rounded-full border border-border-subtle transition-colors disabled:opacity-40 disabled:cursor-not-allowed ${
        checked ? "bg-accent" : "bg-surface"
      }`}
    >
      <span
        className={`inline-block h-3.5 w-3.5 transform rounded-full bg-primary shadow-sm transition-transform ${
          checked ? "translate-x-[18px]" : "translate-x-0.5"
        }`}
      />
    </button>
  );
}

function AppIconChooser({
  variants,
  selected,
  disabled,
  onChange,
}: {
  variants: AppIconVariant[];
  selected: string;
  disabled?: boolean;
  onChange: (id: string) => void;
}) {
  if (variants.length === 0) {
    return (
      <div className="text-[11px] text-dim italic">Loading icons…</div>
    );
  }
  return (
    <div className="flex flex-wrap gap-3">
      {variants.map((v) => {
        const isSelected = v.id === selected;
        return (
          <button
            key={v.id}
            type="button"
            disabled={disabled}
            onClick={() => onChange(v.id)}
            aria-pressed={isSelected}
            aria-label={`Use ${v.label} app icon`}
            title={v.label}
            className={`group relative flex flex-col items-center gap-1.5 rounded-lg p-2 border transition-colors disabled:opacity-40 disabled:cursor-not-allowed ${
              isSelected
                ? "border-accent bg-accent/10"
                : "border-border-subtle bg-surface/40 hover:bg-surface/60 hover:border-border"
            }`}
          >
            <div
              className={`w-16 h-16 rounded-lg overflow-hidden bg-panel border ${
                isSelected ? "border-accent" : "border-border-subtle"
              }`}
            >
              <img
                src={`data:image/png;base64,${v.png_base64}`}
                alt=""
                width={64}
                height={64}
                className="w-full h-full object-contain"
                draggable={false}
              />
            </div>
            <span
              className={`text-[11px] ${
                isSelected ? "text-primary font-medium" : "text-secondary"
              }`}
            >
              {v.label}
            </span>
          </button>
        );
      })}
    </div>
  );
}

function AccentColorChooser({
  value,
  disabled,
  onChange,
}: {
  value: string;
  disabled?: boolean;
  onChange: (hex: string) => void;
}) {
  const colorInputRef = useRef<HTMLInputElement | null>(null);
  const normalized = (normalizeHex(value) ?? FLIPPER_ORANGE).toLowerCase();
  const matchedPreset = ACCENT_PRESETS.find(
    (p) => p.hex.toLowerCase() === normalized,
  );
  const isCustom = !matchedPreset;

  return (
    <div className="flex flex-wrap items-center gap-2">
      {ACCENT_PRESETS.map((p) => {
        const isSelected = p.hex.toLowerCase() === normalized;
        return (
          <button
            key={p.id}
            type="button"
            disabled={disabled}
            onClick={() => onChange(p.hex)}
            aria-pressed={isSelected}
            aria-label={`Use ${p.label} accent color`}
            title={p.label}
            className={`relative h-7 w-7 rounded-full border-2 transition-transform disabled:opacity-40 disabled:cursor-not-allowed ${
              isSelected
                ? "border-primary scale-105"
                : "border-border-subtle hover:border-secondary"
            }`}
            style={{ backgroundColor: p.hex }}
          />
        );
      })}

      <button
        type="button"
        disabled={disabled}
        onClick={() => colorInputRef.current?.click()}
        aria-label="Choose a custom accent color"
        title={isCustom ? `Custom (${normalized})` : "Custom color"}
        className={`relative h-7 w-7 rounded-full border-2 overflow-hidden transition-transform disabled:opacity-40 disabled:cursor-not-allowed ${
          isCustom
            ? "border-primary scale-105"
            : "border-border-subtle hover:border-secondary"
        }`}
        style={
          isCustom
            ? { backgroundColor: normalized }
            : {
                background:
                  "conic-gradient(from 0deg, #ef4444, #f59e0b, #facc15, #22c55e, #06b6d4, #3b82f6, #a855f7, #ec4899, #ef4444)",
              }
        }
      />

      <input
        ref={colorInputRef}
        type="color"
        value={normalized}
        disabled={disabled}
        onChange={(e) => onChange(e.target.value)}
        aria-hidden
        tabIndex={-1}
        className="sr-only absolute pointer-events-none"
      />

      {isCustom && (
        <code className="text-[11px] text-dim font-mono ml-1">{normalized}</code>
      )}
    </div>
  );
}

function DirectoryPicker({
  value,
  disabled,
  onChange,
  ariaLabel,
}: {
  value: string | null;
  disabled?: boolean;
  onChange: (next: string | null) => void;
  ariaLabel: string;
}) {
  const pick = async () => {
    const selected = await openDialog({
      directory: true,
      multiple: false,
      defaultPath: value ?? undefined,
    });
    if (typeof selected === "string") onChange(selected);
  };

  return (
    <div className="flex items-center gap-1.5 max-w-[260px]">
      <button
        type="button"
        onClick={pick}
        disabled={disabled}
        aria-label={ariaLabel}
        title={value ?? "OS default"}
        className="flex items-center gap-1.5 px-2 py-1 text-xs text-secondary hover:text-primary border border-border-subtle rounded hover:bg-surface/60 disabled:opacity-40 disabled:cursor-not-allowed min-w-0"
      >
        {value ? (
          <FolderOpen size={12} className="shrink-0" />
        ) : (
          <Folder size={12} className="shrink-0" />
        )}
        <span className="truncate font-mono text-[11px]">
          {value ?? "OS default"}
        </span>
      </button>
      {value && (
        <button
          type="button"
          onClick={() => onChange(null)}
          disabled={disabled}
          aria-label="Clear folder"
          title="Clear folder"
          className="p-1 text-muted hover:text-danger rounded disabled:opacity-40 disabled:cursor-not-allowed"
        >
          <X size={11} />
        </button>
      )}
    </div>
  );
}

/**
 * Editor for an absolute-Flipper-path list. Accepts any path under `/ext`,
 * `/int`, or `/any`. Used by the Apps section for the "additional app
 * directories" list — the per-library *exclusion* lists are handled by
 * `LibraryExclusionsEditor`, which understands each library's allowed roots.
 */
function AbsoluteDirListEditor({
  heading,
  description,
  placeholder,
  value,
  disabled,
  reserved,
  onChange,
}: {
  heading: string;
  description: string;
  placeholder: string;
  value: string[];
  disabled: boolean;
  /** Paths that cannot be added (e.g. implicit defaults). */
  reserved?: string[];
  onChange: (next: string[]) => void;
}) {
  const [draft, setDraft] = useState("");
  const [validationError, setValidationError] = useState<string | null>(null);
  const datalistId = useId();
  const suggestions = useDirectorySuggestions(draft, "/ext", {
    exclude: [...value, ...(reserved ?? [])],
  });

  const add = () => {
    const trimmed = draft.trim().replace(/\/+$/, "");
    if (!trimmed) return;
    const ok =
      trimmed.startsWith("/ext/") ||
      trimmed.startsWith("/int/") ||
      trimmed.startsWith("/any/") ||
      trimmed === "/ext" ||
      trimmed === "/int" ||
      trimmed === "/any";
    if (!ok) {
      setValidationError("Path must start with /ext, /int, or /any");
      return;
    }
    if (trimmed.includes("..")) {
      setValidationError("Path traversal (..) is not allowed");
      return;
    }
    if (reserved?.includes(trimmed)) {
      setValidationError(`${trimmed} is already scanned by default`);
      return;
    }
    if (value.includes(trimmed)) {
      setValidationError("Already in the list");
      return;
    }
    setValidationError(null);
    setDraft("");
    onChange([...value, trimmed].sort());
  };

  const remove = (path: string) => {
    onChange(value.filter((p) => p !== path));
  };

  return (
    <div className="flex flex-col gap-2">
      <div className="flex flex-col">
        <span className="text-xs text-primary">{heading}</span>
        <span className="text-[11px] text-dim mt-0.5">{description}</span>
      </div>

      <div className="flex items-center gap-2">
        <input
          value={draft}
          onChange={(e) => {
            setDraft(e.target.value);
            if (validationError) setValidationError(null);
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter") add();
          }}
          disabled={disabled}
          placeholder={placeholder}
          list={datalistId}
          autoComplete="off"
          className="flex-1 bg-surface border border-border-subtle rounded px-2 py-1 text-xs text-primary placeholder:text-dim focus:outline-none focus:border-accent disabled:opacity-50"
        />
        <datalist id={datalistId}>
          {suggestions.map((p) => (
            <option key={p} value={p} />
          ))}
        </datalist>
        <button
          onClick={add}
          disabled={disabled || !draft.trim()}
          className="flex items-center gap-1 px-2 py-1 text-xs text-secondary hover:text-primary border border-border-subtle rounded hover:bg-surface/60 disabled:opacity-40 disabled:cursor-not-allowed"
        >
          <Plus size={12} />
          Add
        </button>
      </div>
      {validationError && (
        <span className="text-[11px] text-danger">{validationError}</span>
      )}

      {value.length === 0 ? (
        <span className="text-[11px] text-dim italic">No paths configured.</span>
      ) : (
        <ul className="flex flex-col gap-1">
          {value.map((path) => (
            <li
              key={path}
              className="flex items-center justify-between gap-2 px-2 py-1 bg-surface/50 border border-border-subtle rounded"
            >
              <code className="text-xs text-secondary truncate">{path}</code>
              <button
                onClick={() => remove(path)}
                aria-label={`Remove ${path}`}
                className="p-0.5 text-muted hover:text-danger rounded"
              >
                <X size={11} />
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
