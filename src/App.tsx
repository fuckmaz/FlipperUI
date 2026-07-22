import { useEffect, useRef } from "react";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { connectBleDevice } from "./lib/tauri";
import { DevicePanel } from "./components/DevicePanel/DevicePanel";
import { FileBrowser } from "./components/FileBrowser/FileBrowser";
import { CliPanel } from "./components/CliPanel/CliPanel";
import { ScreenViewer } from "./components/ScreenViewer/ScreenViewer";
import { SettingsPane } from "./components/Settings/SettingsPane";
import { SubGhzLibrary } from "./components/SubGhzLibrary/SubGhzLibrary";
import { InfraredLibrary } from "./components/InfraredLibrary/InfraredLibrary";
import { NfcLibrary } from "./components/NfcLibrary/NfcLibrary";
import { RfidLibrary } from "./components/RfidLibrary/RfidLibrary";
import { BadUsbLibrary } from "./components/BadUsbLibrary/BadUsbLibrary";
import { AppLibrary } from "./components/AppLibrary/AppLibrary";
import { GpioView } from "./components/Gpio/GpioView";
import { DeviceInfoView } from "./components/DeviceInfo/DeviceInfoView";
import { Dashboard } from "./components/Dashboard/Dashboard";
import { CommandPalette } from "./components/CommandPalette/CommandPalette";
import { SideRail } from "./components/Nav/SideRail";
import { useFlipperStore, type ActiveView } from "./store/useFlipperStore";
import { loadSettings, subscribeSettings, updateSettings } from "./lib/settings";
import { connectPreferredAvailableUsbDevice } from "./lib/usbConnection";
import { applyAccentColor } from "./lib/theme";
import { syncClockOnConnectIfEnabled } from "./lib/clockSync";
import { notify } from "./lib/notify";
import { usePreloadLibraries } from "./hooks/usePreloadLibraries";
import flipperOutlineUrl from "./assets/flipper-outline.svg";
import { ErrorBanner } from "./components/ui/ErrorBanner";
import { AppUpdateNotice } from "./components/AppUpdate/AppUpdateNotice";
import { resolveFeatureAvailability } from "./lib/capabilities";
import type { DeviceCapabilities } from "./types/flipper";
import { deviceTelemetry } from "./lib/deviceTelemetry";

export default function App() {
  const activeView = useFlipperStore((s) => s.activeView);
  const setActiveView = useFlipperStore((s) => s.setActiveView);
  const isConnected = useFlipperStore((s) => s.isConnected);
  const deviceInfo = useFlipperStore((s) => s.deviceInfo);
  const setConnected = useFlipperStore((s) => s.setConnected);
  const setError = useFlipperStore((s) => s.setError);

  usePreloadLibraries();

  // One app-lifetime owner drives all low-frequency device telemetry. Views
  // subscribe to the same snapshot instead of starting competing RPC timers.
  useEffect(() => {
    deviceTelemetry.setConnection(
      isConnected && deviceInfo
        ? `${deviceInfo.hardware_uid ?? "unknown"}:${deviceInfo.port}`
        : null,
    );
  }, [deviceInfo, isConnected]);

  // Close the splash window and show the main window once React has mounted.
  // We can't gate on rAF here because the main window starts with
  // `visible: false`, and macOS WebKit throttles requestAnimationFrame in
  // windows that have never been shown — the callback would never fire and
  // the splash would stick forever. Invoking directly from the mount effect
  // is good enough: by the time this runs, the DOM has been rendered and the
  // webview is ready to paint as soon as the OS makes it visible.
  useEffect(() => {
    void invoke("close_splashscreen").catch(() => {});
  }, []);

  // Apply persisted system-UI preferences (tray icon, dock visibility) on
  // every startup. The Rust side creates the tray by default and leaves the
  // dock icon visible, so we only need to call through when the stored
  // settings differ from those defaults.
  // Apply the persisted accent color as early as possible, and re-apply on
  // every settings change. Done in its own effect (without deps) so it runs
  // exactly once on mount and the subscription survives for the app lifetime.
  useEffect(() => {
    loadSettings()
      .then((s) => applyAccentColor(s.appearance.themeAccent))
      .catch(() => {});
    return subscribeSettings((s) => applyAccentColor(s.appearance.themeAccent));
  }, []);

  useEffect(() => {
    loadSettings()
      .then(async (s) => {
        if (!s.tray.enabled) {
          await invoke("set_tray_enabled", { enabled: false }).catch(() => {});
        } else if (s.tray.monochromeIcon) {
          await invoke("set_tray_monochrome", { monochrome: true }).catch(
            () => {},
          );
        }
        if (s.tray.enabled && s.tray.hideDockIcon) {
          await invoke("set_dock_visible", { visible: false }).catch(() => {});
        }
      })
      .catch(() => {});
  }, []);

  // The Rust side emits `flipper-disconnected` when the screen-stream reader
  // (or any other background worker) tears the client down after an
  // unrecoverable serial error. Without this the UI would keep showing
  // "connected" over a dead link.
  //
  // After teardown we may attempt a single auto-reconnect against the
  // last-used transport (BLE id or USB port), with backoff and a small retry
  // budget. The reconnect is gated behind the `connection.autoReconnect`
  // setting — when off, we just clear the connection and surface the error.
  // Manual disconnects via the DevicePanel skip this path entirely because
  // they don't go through `flipper-disconnected`.
  const reconnectAttemptsRef = useRef(0);
  const reconnectTimerRef = useRef<number | null>(null);
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;

    const cancel = () => {
      if (reconnectTimerRef.current !== null) {
        window.clearTimeout(reconnectTimerRef.current);
        reconnectTimerRef.current = null;
      }
    };

    const tryReconnect = async () => {
      if (cancelled) return;
      const settings = await loadSettings().catch(() => null);
      if (!settings || cancelled) return;
      // Setting may have flipped to off mid-backoff — abort cleanly.
      if (!settings.connection.autoReconnect) return;
    
      if (useFlipperStore.getState().isConnected) {
        reconnectAttemptsRef.current = 0;
        return;
      }

      const { transport, lastPort, lastBleId, lastBleName } = settings.connection;
      const maybeSyncClockAfterConnect = async (): Promise<string | null> => {
        try {
          await syncClockOnConnectIfEnabled();
          return null;
        } catch (e) {
          return `Clock sync failed: ${e instanceof Error ? e.message : String(e)}`;
        }
      };
      try {
        if (transport === "ble" && lastBleId) {
          const info = await connectBleDevice(lastBleId, lastBleName ?? undefined);
          const clockError = await maybeSyncClockAfterConnect();
          setConnected(info, "ble");
          setError(clockError);
          reconnectAttemptsRef.current = 0;
          return;
        }
        if (transport === "usb") {
          const info = await connectPreferredAvailableUsbDevice(lastPort);
          const clockError = await maybeSyncClockAfterConnect();
          void updateSettings({ connection: { lastPort: info.port } }).catch(() => {});
          setConnected(info);
          setError(clockError);
          reconnectAttemptsRef.current = 0;
          return;
        }
      } catch {
        
      }

      // Exponential backoff capped at ~10s, give up after 5 tries.
      reconnectAttemptsRef.current += 1;
      if (reconnectAttemptsRef.current >= 5) return;
      const delay = Math.min(10_000, 1_000 * 2 ** (reconnectAttemptsRef.current - 1));
      reconnectTimerRef.current = window.setTimeout(tryReconnect, delay);
    };

    listen<string>("flipper-disconnected", async (event) => {
      setConnected(null);
      void notify("deviceDisconnected", "Flipper disconnected", event.payload);
      cancel();
      reconnectAttemptsRef.current = 0;
      const settings = await loadSettings().catch(() => null);
      if (cancelled) return;
      if (!settings?.connection.autoReconnect) {
        setError(`Disconnected: ${event.payload}`);
        return;
      }
      setError(`Disconnected: ${event.payload} — reconnecting…`);
      // Initial 500ms grace so the OS has time to release the serial port / BLE peripheral
      reconnectTimerRef.current = window.setTimeout(tryReconnect, 500);
    }).then((u) => {
      if (cancelled) u();
      else unlisten = u;
    });
    return () => {
      cancelled = true;
      cancel();
      unlisten?.();
    };
  }, [setConnected, setError]);

  // Native menu (Cmd+, / FlipperUI → Settings…) navigates to the Settings view.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    listen("open-settings", () => setActiveView("settings")).then((u) => {
      if (cancelled) u();
      else unlisten = u;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [setActiveView]);

  // Tray flyout navigation shortcuts emit "tray-nav" with the view name.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    listen<string>("tray-nav", (event) => {
      const view = event.payload as ActiveView;
      setActiveView(view);
    }).then((u) => {
      if (cancelled) u();
      else unlisten = u;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [setActiveView]);

  return (
    <div className="flex h-screen bg-app text-primary overflow-hidden select-none">
      <SideRail />
      <div className="flex-1 min-w-0 flex flex-col overflow-hidden">
        <DevicePanel />
        <ErrorBanner />
        <ActivePane activeView={activeView} isConnected={isConnected} />
      </div>
      <CommandPalette />
      <AppUpdateNotice />
    </div>
  );
}

function ActivePane({
  activeView,
  isConnected,
}: {
  activeView: ActiveView;
  isConnected: boolean;
}) {
  const subghzCount = useFlipperStore((s) => s.subghzEntries.length);
  const irCount = useFlipperStore((s) => s.irEntries.length);
  const nfcCount = useFlipperStore((s) => s.nfcEntries.length);
  const rfidCount = useFlipperStore((s) => s.rfidEntries.length);
  const badusbCount = useFlipperStore((s) => s.badusbEntries.length);
  const deviceInfo = useFlipperStore((s) => s.deviceInfo);

  if (activeView === "settings") {
    return <SettingsPane />;
  }

  // Dashboard works while disconnected
  if (activeView === "dashboard") {
    return <Dashboard />;
  }

  // Cached libraries stay browsable while offline.
  if (activeView === "subghz" && (isConnected || subghzCount > 0)) {
    return <SubGhzLibrary />;
  }
  if (activeView === "infrared" && (isConnected || irCount > 0)) {
    return <InfraredLibrary />;
  }
  if (activeView === "nfc" && (isConnected || nfcCount > 0)) {
    return <NfcLibrary />;
  }
  if (activeView === "rfid" && (isConnected || rfidCount > 0)) {
    return <RfidLibrary />;
  }
  if (activeView === "badusb" && (isConnected || badusbCount > 0)) {
    return <BadUsbLibrary />;
  }

  if (!isConnected) {
    return <DisconnectedEmptyState />;
  }

  const capabilityByView: Partial<Record<ActiveView, keyof DeviceCapabilities>> = {
    files: "storage",
    apps: "storage",
    gpio: "gpio",
    cli: "cli",
    screen: "screen_stream",
  };
  const requiredCapability = capabilityByView[activeView];
  if (requiredCapability) {
    const availability = resolveFeatureAvailability(
      deviceInfo?.capabilities[requiredCapability],
    );
    if (!availability.available) {
      return (
        <FeatureUnavailable
          feature={activeView === "cli" ? "Terminal" : activeView}
          reason={availability.reason}
        />
      );
    }
  }

  if (activeView === "apps") return <AppLibrary />;
  if (activeView === "gpio") return <GpioView />;
  if (activeView === "info") return <DeviceInfoView />;
  if (activeView === "cli") return <CliPanel />;
  if (activeView === "screen") return <ScreenViewer />;

  return (
    <div className="flex-1 min-h-0 overflow-hidden flex flex-col">
      <FileBrowser />
    </div>
  );
}

function FeatureUnavailable({ feature, reason }: { feature: string; reason: string }) {
  return (
    <div className="flex-1 flex flex-col items-center justify-center gap-2 px-6 text-center">
      <p className="text-sm font-medium text-primary capitalize">{feature} unavailable</p>
      <p className="max-w-md text-xs text-muted">{reason}</p>
    </div>
  );
}

function DisconnectedEmptyState() {
  return (
    <div className="flex-1 flex flex-col items-center justify-center gap-4 text-dim">
      <div
        aria-hidden
        className="text-elevated"
        style={{
          width: 240,
          height: 178,
          backgroundColor: "currentColor",
          WebkitMaskImage: `url(${flipperOutlineUrl})`,
          maskImage: `url(${flipperOutlineUrl})`,
          WebkitMaskRepeat: "no-repeat",
          maskRepeat: "no-repeat",
          WebkitMaskPosition: "center",
          maskPosition: "center",
          WebkitMaskSize: "contain",
          maskSize: "contain",
        }}
      />
      <p className="text-sm">Connect a Flipper Zero to get started</p>
    </div>
  );
}
