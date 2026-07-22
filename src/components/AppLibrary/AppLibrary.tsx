import { useCallback, useEffect, useMemo, useState } from "react";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { AlertTriangle, LayoutGrid, Upload } from "lucide-react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { useFlipperStore } from "../../store/useFlipperStore";
import {
  appsCancelScan,
  appsParsePaths,
  appsReadIcon,
  appsScan,
  storageWriteFromLocal,
} from "../../lib/tauri";
import { loadSettings, subscribeSettings } from "../../lib/settings";
import { loadAppsCache, saveAppIcons, saveAppsCache } from "../../lib/appsCache";
import { notify } from "../../lib/notify";
import { commandErrorMessage, isCommandCancelled } from "../../lib/commandError";
import { LibraryToolbar } from "./LibraryToolbar";
import { LibraryTable } from "./LibraryTable";
import { basename } from "../../lib/path";

const DEFAULT_ROOT = "/ext/apps";

export function AppLibrary() {
  const isConnected = useFlipperStore((s) => s.isConnected);
  const deviceUid = useFlipperStore((s) => s.deviceInfo?.hardware_uid ?? null);
  const entries = useFlipperStore((s) => s.appEntries);
  const scanning = useFlipperStore((s) => s.appsScanning);
  const error = useFlipperStore((s) => s.appsError);
  const setEntries = useFlipperStore((s) => s.setAppEntries);
  const setScanning = useFlipperStore((s) => s.setAppsScanning);
  const setProgress = useFlipperStore((s) => s.setAppsProgress);
  const setError = useFlipperStore((s) => s.setAppsError);
  const setAppIcons = useFlipperStore((s) => s.setAppIcons);
  const setAppIcon = useFlipperStore((s) => s.setAppIcon);

  const [excludedDirs, setExcludedDirs] = useState<string[]>([]);
  const [extraDirs, setExtraDirs] = useState<string[]>([]);
  const [query, setQuery] = useState("");
  const [categoryFilter, setCategoryFilter] = useState<string | null>(null);
  const [cacheScannedAt, setCacheScannedAt] = useState<number | null>(null);
  const [isDragOver, setIsDragOver] = useState(false);
  const [installingCount, setInstallingCount] = useState<number>(0);

  useEffect(() => {
    loadSettings().then((s) => {
      setExcludedDirs(s.apps.excludedDirs);
      setExtraDirs(s.apps.extraDirs);
    });
    return subscribeSettings((s) => {
      setExcludedDirs(s.apps.excludedDirs);
      setExtraDirs(s.apps.extraDirs);
    });
  }, []);

  const injection = useFlipperStore((s) => s.librarySearchInjection);
  const clearInjection = useFlipperStore((s) => s.setLibrarySearchInjection);
  useEffect(() => {
    if (injection && injection.view === "apps") {
      setQuery(injection.query);
      setCategoryFilter(null);
      clearInjection(null);
    }
  }, [injection, clearInjection]);

  useEffect(() => {
    if (!deviceUid) return;
    let cancelled = false;
    loadAppsCache(deviceUid).then((cache) => {
      if (cancelled || !cache) return;
      setCacheScannedAt(cache.scannedAt);
      if (useFlipperStore.getState().appEntries.length === 0) {
        setEntries(cache.entries);
      }
      // Hydrate icons from the persisted cache — any entries still missing
      // an icon (or whose mtime advanced since last fetch) will be picked up
      // by the prefetch effect below.
      setAppIcons(cache.icons ?? {});
    });
    return () => {
      cancelled = true;
    };
  }, [deviceUid, setEntries, setAppIcons]);

  // Background icon prefetch: for any entry whose mtime moved past the
  // cached icon's mtime (or that has no cached icon), read the .fap and
  // extract the embedded 10x10 bitmap. Serial by design — the Rust side
  // holds the client mutex for the duration of each read, and interleaving
  // with user-initiated RPC would mean one icon takes ~1s on a 230400-baud
  // link. User actions naturally queue behind each fetch via the mutex.
  useEffect(() => {
    if (!deviceUid || entries.length === 0) return;
    let cancelled = false;

    const run = async () => {
      for (const entry of entries) {
        if (cancelled) return;
        const cached = useFlipperStore.getState().appIcons[entry.path];
        if (cached && cached.mtime === entry.mtime) continue;
        if (!useFlipperStore.getState().isConnected) return;
        try {
          const icon = await appsReadIcon(entry.path);
          if (cancelled) return;
          setAppIcon(entry.path, { icon, mtime: entry.mtime });
          // Persist after each fetch — if the user navigates away or the
          // device drops mid-prefetch, we keep whatever we already got.
          const snapshot = useFlipperStore.getState().appIcons;
          await saveAppIcons(deviceUid, snapshot).catch(() => {});
        } catch {
          // Per-file failure shouldn't block the rest of the queue. A CLI-
          // mode switch, disconnect, or transient RPC error will just skip
          // this entry; the next mount will retry.
          if (cancelled) return;
        }
      }
    };

    run();
    return () => {
      cancelled = true;
    };
  }, [deviceUid, entries, setAppIcon]);

  const roots = useMemo(() => {
    const set = new Set<string>([DEFAULT_ROOT, ...extraDirs]);
    return Array.from(set);
  }, [extraDirs]);

  const runScan = useCallback(async () => {
    if (scanning) return;
    setError(null);
    setScanning(true);
    setProgress({ scanned: 0, total: 0, current_path: "" });
    try {
      const list = await appsScan(roots, excludedDirs, entries, setProgress);
      setEntries(list);
      if (deviceUid) {
        await saveAppsCache(deviceUid, list).catch(() => {});
        setCacheScannedAt(Date.now());
      }
      void notify("libraryScan", "App scan complete", `${list.length} apps indexed.`);
    } catch (e) {
      if (!isCommandCancelled(e)) {
        setError(`Scan failed: ${commandErrorMessage(e, "apps_scan")}`);
      }
    } finally {
      setScanning(false);
      setProgress(null);
    }
  }, [
    scanning,
    setError,
    setScanning,
    setProgress,
    roots,
    excludedDirs,
    entries,
    setEntries,
    deviceUid,
  ]);

  const cancelScan = async () => {
    try {
      await appsCancelScan();
    } catch {
      /* fine */
    }
  };

  const installFap = useCallback(
    async (localPath: string): Promise<string | null> => {
      const name = basename(localPath);
      if (!name.toLowerCase().endsWith(".fap")) {
        setError(`Skipped "${name}" — only .fap files can be installed.`);
        return null;
      }
      const remotePath = `${DEFAULT_ROOT}/${name}`;
      await storageWriteFromLocal(remotePath, localPath);
      return remotePath;
    },
    [setError],
  );

  // Incremental merge after install: parse only the freshly-written .fap
  // paths and splice them into existing entries (replacing same-path
  // overwrites). Avoids re-walking every apps root after a single drop.
  const mergeInstalled = useCallback(
    async (uploadedPaths: string[]) => {
      if (uploadedPaths.length === 0) return;
      try {
        const parsed = await appsParsePaths(uploadedPaths, roots);
        if (parsed.length === 0) return;
        const current = useFlipperStore.getState().appEntries;
        const byPath = new Map(current.map((e) => [e.path, e]));
        for (const e of parsed) byPath.set(e.path, e);
        const next = Array.from(byPath.values());
        setEntries(next);
        if (deviceUid) {
          await saveAppsCache(deviceUid, next).catch(() => {});
          setCacheScannedAt(Date.now());
        }
      } catch (e) {
        setError(`Couldn't parse installed apps: ${(e as Error).message || String(e)}`);
      }
    },
    [deviceUid, roots, setEntries, setError],
  );

  const installMany = useCallback(
    async (paths: string[]) => {
      const faps = paths.filter((p) => p.toLowerCase().endsWith(".fap"));
      if (faps.length === 0) {
        setError("No .fap files in the drop — nothing to install.");
        return;
      }
      setInstallingCount(faps.length);
      setError(null);
      const uploaded: string[] = [];
      try {
        for (const p of faps) {
          const remote = await installFap(p);
          if (remote) uploaded.push(remote);
        }
        await mergeInstalled(uploaded);
      } catch (e) {
        setError(`Install failed: ${(e as Error).message || String(e)}`);
      } finally {
        setInstallingCount(0);
      }
    },
    [installFap, mergeInstalled, setError],
  );

  // Drag-and-drop .fap install
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    getCurrentWebviewWindow()
      .onDragDropEvent((event) => {
        if (cancelled) return;
        if (event.payload.type === "enter") setIsDragOver(true);
        else if (event.payload.type === "leave") setIsDragOver(false);
        else if (event.payload.type === "drop") {
          setIsDragOver(false);
          void installMany(event.payload.paths);
        }
      })
      .then((fn) => {
        if (cancelled) fn();
        else unlisten = fn;
      });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [installMany]);

  const onUploadClick = async () => {
    const selected = await openDialog({
      multiple: true,
      filters: [{ name: "Flipper apps", extensions: ["fap"] }],
    });
    if (!selected) return;
    const paths = Array.isArray(selected) ? selected : [selected];
    await installMany(paths);
  };

  const categories = useMemo(() => {
    const set = new Set<string>();
    for (const e of entries) {
      if (e.category) set.add(e.category);
    }
    return Array.from(set).sort((a, b) => a.localeCompare(b));
  }, [entries]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return entries.filter((e) => {
      if (categoryFilter && e.category !== categoryFilter) return false;
      if (!q) return true;
      const haystack = [e.name, e.path, e.category ?? "", e.root]
        .join(" ")
        .toLowerCase();
      return haystack.includes(q);
    });
  }, [entries, query, categoryFilter]);

  if (!isConnected) return null;

  return (
    <div className="flex-1 min-h-0 flex flex-col overflow-hidden relative">
      <LibraryToolbar
        roots={roots}
        categories={categories}
        categoryFilter={categoryFilter}
        onCategoryFilterChange={setCategoryFilter}
        query={query}
        onQueryChange={setQuery}
        onRefresh={runScan}
        onCancel={cancelScan}
        onUpload={onUploadClick}
        installingCount={installingCount}
        total={entries.length}
        filtered={filtered.length}
        lastScannedAt={cacheScannedAt}
      />
      {error && (
        <div className="flex items-start gap-2 px-3 py-2 bg-danger/10 border-b border-danger/30 text-xs text-danger">
          <AlertTriangle size={13} className="mt-0.5 shrink-0" />
          <span className="flex-1">{error}</span>
          <button
            onClick={() => setError(null)}
            className="text-danger/70 hover:text-danger"
          >
            ×
          </button>
        </div>
      )}
      {entries.length === 0 && !scanning ? (
        <EmptyState onScan={runScan} roots={roots} />
      ) : (
        <LibraryTable entries={filtered} allEntries={entries} />
      )}

      {isDragOver && (
        <div className="absolute inset-0 z-40 flex items-center justify-center bg-app/60 backdrop-blur-sm">
          <div className="flex flex-col items-center gap-3 px-8 py-6 border-2 border-dashed border-accent/60 rounded-xl">
            <Upload size={32} className="text-accent" />
            <span className="text-sm text-accent/80 font-medium">
              Drop .fap files to install into {DEFAULT_ROOT}
            </span>
          </div>
        </div>
      )}
    </div>
  );
}

function EmptyState({ onScan, roots }: { onScan: () => void; roots: string[] }) {
  return (
    <div className="flex-1 flex flex-col items-center justify-center gap-3 text-dim">
      <LayoutGrid size={40} strokeWidth={1.5} className="text-elevated" />
      <p className="text-sm">No apps indexed yet.</p>
      <p className="text-[11px] text-dim max-w-md text-center">
        Scans: {roots.join(", ")}
      </p>
      <button
        onClick={onScan}
        className="px-3 py-1.5 text-xs text-primary bg-accent/20 border border-accent/40 rounded hover:bg-accent/30"
      >
        Scan now
      </button>
    </div>
  );
}
