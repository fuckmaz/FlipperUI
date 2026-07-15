import { create } from "zustand";
import type { DeviceInfo, FileEntry, PortInfo } from "../types/flipper";
import type { ScanProgress, SubGhzEntry } from "../types/subghz";
import type { IrEntry, IrScanProgress } from "../types/infrared";
import type { NfcEntry, NfcScanProgress } from "../types/nfc";
import type { RfidEntry, RfidScanProgress } from "../types/rfid";
import type { BadUsbEntry, BadUsbScanProgress } from "../types/badusb";
import type { AppEntry, AppIconEntry, AppScanProgress } from "../types/apps";

export type ActiveView =
  | "dashboard"
  | "files"
  | "subghz"
  | "infrared"
  | "nfc"
  | "rfid"
  | "badusb"
  | "apps"
  | "gpio"
  | "info"
  | "cli"
  | "screen"
  | "settings";

export type ConnectionKind = "serial" | "ble";

interface FlipperStore {
  // Navigation
  activeView: ActiveView;

  // Device state
  ports: PortInfo[];
  selectedPort: string | null;
  deviceInfo: DeviceInfo | null;
  isConnected: boolean;
  isConnecting: boolean;
  /** Which transport the current session uses. Null when disconnected. */
  connectionKind: ConnectionKind | null;

  // File browser state
  currentPath: string;
  entries: FileEntry[];
  isLoading: boolean;
  error: string | null;
  /** True while one confirmed file-browser delete batch owns the backend client. */
  fileBrowserDeleting: boolean;

  // Transfer state (0–100, null when idle)
  transferProgress: number | null;

  // CLI state
  cliConnected: boolean;
  cliHistory: Array<{ id: number; type: "input" | "output" | "error"; text: string }>;

  // Sub-GHz library
  subghzEntries: SubGhzEntry[];
  subghzScanning: boolean;
  subghzProgress: ScanProgress | null;
  subghzError: string | null;
  /** Path of the .sub file currently being transmitted, or null when idle. */
  subghzTransmittingPath: string | null;
  /** Starred .sub paths for the current device. Hydrated from cache. */
  subghzFavorites: string[];

  // Infrared library
  irEntries: IrEntry[];
  irScanning: boolean;
  irProgress: IrScanProgress | null;
  irError: string | null;

  // NFC library
  nfcEntries: NfcEntry[];
  nfcScanning: boolean;
  nfcProgress: NfcScanProgress | null;
  nfcError: string | null;

  // RFID library (125 kHz)
  rfidEntries: RfidEntry[];
  rfidScanning: boolean;
  rfidProgress: RfidScanProgress | null;
  rfidError: string | null;

  // BadUSB library
  badusbEntries: BadUsbEntry[];
  badusbScanning: boolean;
  badusbProgress: BadUsbScanProgress | null;
  badusbError: string | null;

  // Pending search query injected by the GlobalSearch bar. Each library
  // component reads this on mount / when activeView matches and applies it to
  // its local query state, then clears it. Single-shot — not a persistent
  // filter for the library.
  librarySearchInjection: { view: ActiveView; query: string } | null;

  // App library
  appEntries: AppEntry[];
  appsScanning: boolean;
  appsProgress: AppScanProgress | null;
  appsError: string | null;
  /** Path of the .fap currently being launched on-device, or null when idle. */
  appsLaunchingPath: string | null;
  /** Lazy-loaded per-app icons, keyed by .fap path. */
  appIcons: Record<string, AppIconEntry>;

  // Actions
  setActiveView: (view: ActiveView) => void;
  setPorts: (ports: PortInfo[]) => void;
  setSelectedPort: (port: string | null) => void;
  setConnecting: (connecting: boolean) => void;
  setConnected: (info: DeviceInfo | null, kind?: ConnectionKind | null) => void;
  setCurrentPath: (path: string) => void;
  setEntries: (entries: FileEntry[]) => void;
  setLoading: (loading: boolean) => void;
  setError: (error: string | null) => void;
  setFileBrowserDeleting: (deleting: boolean) => void;
  setTransferProgress: (progress: number | null) => void;
  setCliConnected: (connected: boolean) => void;
  addCliLine: (line: { type: "input" | "output" | "error"; text: string }) => void;
  clearCli: () => void;
  setSubghzEntries: (entries: SubGhzEntry[]) => void;
  setSubghzScanning: (scanning: boolean) => void;
  setSubghzProgress: (progress: ScanProgress | null) => void;
  setSubghzError: (error: string | null) => void;
  setSubghzTransmittingPath: (path: string | null) => void;
  setSubghzFavorites: (favorites: string[]) => void;
  toggleSubghzFavorite: (path: string) => void;
  setIrEntries: (entries: IrEntry[]) => void;
  setIrScanning: (scanning: boolean) => void;
  setIrProgress: (progress: IrScanProgress | null) => void;
  setIrError: (error: string | null) => void;
  setNfcEntries: (entries: NfcEntry[]) => void;
  setNfcScanning: (scanning: boolean) => void;
  setNfcProgress: (progress: NfcScanProgress | null) => void;
  setNfcError: (error: string | null) => void;
  setRfidEntries: (entries: RfidEntry[]) => void;
  setRfidScanning: (scanning: boolean) => void;
  setRfidProgress: (progress: RfidScanProgress | null) => void;
  setRfidError: (error: string | null) => void;
  setBadUsbEntries: (entries: BadUsbEntry[]) => void;
  setBadUsbScanning: (scanning: boolean) => void;
  setBadUsbProgress: (progress: BadUsbScanProgress | null) => void;
  setBadUsbError: (error: string | null) => void;
  setAppEntries: (entries: AppEntry[]) => void;
  setAppsScanning: (scanning: boolean) => void;
  setAppsProgress: (progress: AppScanProgress | null) => void;
  setAppsError: (error: string | null) => void;
  setAppsLaunchingPath: (path: string | null) => void;
  setAppIcons: (icons: Record<string, AppIconEntry>) => void;
  setAppIcon: (path: string, entry: AppIconEntry) => void;
  setLibrarySearchInjection: (
    injection: { view: ActiveView; query: string } | null,
  ) => void;
}

let cliLineId = 0;

export const useFlipperStore = create<FlipperStore>((set) => ({
  activeView: "dashboard",
  ports: [],
  selectedPort: null,
  deviceInfo: null,
  isConnected: false,
  isConnecting: false,
  connectionKind: null,
  currentPath: "/ext",
  entries: [],
  isLoading: false,
  error: null,
  fileBrowserDeleting: false,
  transferProgress: null,
  cliConnected: false,
  cliHistory: [],
  subghzEntries: [],
  subghzScanning: false,
  subghzProgress: null,
  subghzError: null,
  subghzTransmittingPath: null,
  subghzFavorites: [],
  irEntries: [],
  irScanning: false,
  irProgress: null,
  irError: null,
  nfcEntries: [],
  nfcScanning: false,
  nfcProgress: null,
  nfcError: null,
  rfidEntries: [],
  rfidScanning: false,
  rfidProgress: null,
  rfidError: null,
  badusbEntries: [],
  badusbScanning: false,
  badusbProgress: null,
  badusbError: null,
  appEntries: [],
  appsScanning: false,
  appsProgress: null,
  appsError: null,
  appsLaunchingPath: null,
  appIcons: {},
  librarySearchInjection: null,

  setActiveView: (activeView) => set({ activeView }),
  setPorts: (ports) => set({ ports }),
  setSelectedPort: (selectedPort) => set({ selectedPort }),
  setConnecting: (isConnecting) => set({ isConnecting }),
  setConnected: (deviceInfo, kind) =>
    set({
      deviceInfo,
      isConnected: deviceInfo !== null,
      isConnecting: false,
      connectionKind: deviceInfo === null ? null : kind ?? "serial",
      // Reset file browser + in-flight scan/transmit state on disconnect.
      // Library *entries* (subghz / ir / nfc / apps) deliberately survive so
      // cached libraries stay browsable offline — they get rehydrated from
      // disk cache the next time a device connects (see each library view's
      // deviceUid effect).
      ...(deviceInfo === null
        ? {
            currentPath: "/ext",
            entries: [],
            error: null,
            fileBrowserDeleting: false,
            cliConnected: false,
            cliHistory: [],
            subghzScanning: false,
            subghzProgress: null,
            subghzError: null,
            subghzTransmittingPath: null,
            irScanning: false,
            irProgress: null,
            irError: null,
            nfcScanning: false,
            nfcProgress: null,
            nfcError: null,
            rfidScanning: false,
            rfidProgress: null,
            rfidError: null,
            badusbScanning: false,
            badusbProgress: null,
            badusbError: null,
            appsScanning: false,
            appsProgress: null,
            appsError: null,
            appsLaunchingPath: null,
          }
        : {}),
    }),
  setCurrentPath: (currentPath) => set({ currentPath }),
  setEntries: (entries) => set({ entries }),
  setLoading: (isLoading) => set({ isLoading }),
  setError: (error) => set({ error }),
  setFileBrowserDeleting: (fileBrowserDeleting) => set({ fileBrowserDeleting }),
  setTransferProgress: (transferProgress) => set({ transferProgress }),
  setCliConnected: (cliConnected) => set({ cliConnected }),
  addCliLine: (line) =>
    set((s) => {
      const MAX_CLI_LINES = 1000; // Reduced from 5000 for memory efficiency
      const entry = { ...line, id: cliLineId++ };
      const history = [...s.cliHistory, entry];
      return { cliHistory: history.length > MAX_CLI_LINES ? history.slice(-MAX_CLI_LINES) : history };
    }),
  clearCli: () => set({ cliHistory: [] }),
  setSubghzEntries: (subghzEntries) => set({ subghzEntries }),
  setSubghzScanning: (subghzScanning) => set({ subghzScanning }),
  setSubghzProgress: (subghzProgress) => set({ subghzProgress }),
  setSubghzError: (subghzError) => set({ subghzError }),
  setSubghzTransmittingPath: (subghzTransmittingPath) =>
    set({ subghzTransmittingPath }),
  setSubghzFavorites: (subghzFavorites) => set({ subghzFavorites }),
  toggleSubghzFavorite: (path) =>
    set((s) => ({
      subghzFavorites: s.subghzFavorites.includes(path)
        ? s.subghzFavorites.filter((p) => p !== path)
        : [...s.subghzFavorites, path],
    })),
  setIrEntries: (irEntries) => set({ irEntries }),
  setIrScanning: (irScanning) => set({ irScanning }),
  setIrProgress: (irProgress) => set({ irProgress }),
  setIrError: (irError) => set({ irError }),
  setNfcEntries: (nfcEntries) => set({ nfcEntries }),
  setNfcScanning: (nfcScanning) => set({ nfcScanning }),
  setNfcProgress: (nfcProgress) => set({ nfcProgress }),
  setNfcError: (nfcError) => set({ nfcError }),
  setRfidEntries: (rfidEntries) => set({ rfidEntries }),
  setRfidScanning: (rfidScanning) => set({ rfidScanning }),
  setRfidProgress: (rfidProgress) => set({ rfidProgress }),
  setRfidError: (rfidError) => set({ rfidError }),
  setBadUsbEntries: (badusbEntries) => set({ badusbEntries }),
  setBadUsbScanning: (badusbScanning) => set({ badusbScanning }),
  setBadUsbProgress: (badusbProgress) => set({ badusbProgress }),
  setBadUsbError: (badusbError) => set({ badusbError }),
  setAppEntries: (appEntries) => set({ appEntries }),
  setAppsScanning: (appsScanning) => set({ appsScanning }),
  setAppsProgress: (appsProgress) => set({ appsProgress }),
  setAppsError: (appsError) => set({ appsError }),
  setAppsLaunchingPath: (appsLaunchingPath) => set({ appsLaunchingPath }),
  setAppIcons: (appIcons) => set({ appIcons }),
  setAppIcon: (path, entry) =>
    set((s) => ({ appIcons: { ...s.appIcons, [path]: entry } })),
  setLibrarySearchInjection: (librarySearchInjection) =>
    set({ librarySearchInjection }),
}));
