import { useEffect, useMemo, useState } from "react";
import { RadioTower, AlertTriangle } from "lucide-react";
import { useFlipperStore } from "../../store/useFlipperStore";
import { subghzCancelScan, subghzScan } from "../../lib/tauri";
import { loadSettings, subscribeSettings, updateSettings } from "../../lib/settings";
import { useLibraryPreScan } from "../../hooks/useLibraryPreScan";
import { notify } from "../../lib/notify";
import { commandErrorMessage, isCommandCancelled } from "../../lib/commandError";
import {
  loadSubghzCache,
  saveSubghzCache,
  saveSubghzFavorites,
} from "../../lib/subghzCache";
import { LibraryToolbar } from "./LibraryToolbar";
import { LibraryTable } from "./LibraryTable";
import type { SubGhzEntry } from "../../types/subghz";
import { subghzFavoriteIdentity } from "../../lib/subghzFavorites";

const SUBGHZ_ROOT = "/ext/subghz";

export function SubGhzLibrary() {
  const isConnected = useFlipperStore((s) => s.isConnected);
  const deviceUid = useFlipperStore((s) => s.deviceInfo?.hardware_uid ?? null);
  const entries = useFlipperStore((s) => s.subghzEntries);
  const scanning = useFlipperStore((s) => s.subghzScanning);
  const error = useFlipperStore((s) => s.subghzError);
  const setEntries = useFlipperStore((s) => s.setSubghzEntries);
  const setScanning = useFlipperStore((s) => s.setSubghzScanning);
  const setProgress = useFlipperStore((s) => s.setSubghzProgress);
  const setError = useFlipperStore((s) => s.setSubghzError);
  const favorites = useFlipperStore((s) => s.subghzFavorites);
  const setFavorites = useFlipperStore((s) => s.setSubghzFavorites);

  const [excludedDirs, setExcludedDirs] = useState<string[]>([]);
  const [query, setQuery] = useState("");
  const [protocolFilter, setProtocolFilter] = useState<string | null>(null);
  const [starredOnly, setStarredOnly] = useState(false);
  const [cacheScannedAt, setCacheScannedAt] = useState<number | null>(null);
  const { checkBeforeScan, modal: preScanModal } = useLibraryPreScan("subghz");

  const toggleFavorite = (entry: SubGhzEntry) => {
    const identity = subghzFavoriteIdentity(entry);
    const next = favorites.includes(identity)
      ? favorites.filter((favorite) => favorite !== identity)
      : [...favorites, identity];
    setFavorites(next);
    if (deviceUid) saveSubghzFavorites(deviceUid, next).catch(() => {});
  };

  // Pull excluded dirs from persisted settings so the scan honors them.
  useEffect(() => {
    loadSettings().then((s) => setExcludedDirs(s.subghz.excludedDirs));
    return subscribeSettings((s) => setExcludedDirs(s.subghz.excludedDirs));
  }, []);

  const injection = useFlipperStore((s) => s.librarySearchInjection);
  const clearInjection = useFlipperStore((s) => s.setLibrarySearchInjection);
  useEffect(() => {
    if (injection && injection.view === "subghz") {
      setQuery(injection.query);
      setProtocolFilter(null);
      clearInjection(null);
    }
  }, [injection, clearInjection]);

  // Hydrate from the on-disk cache the moment we know which device we're on.
  // Fires on every deviceUid change, which includes reconnecting — fine
  // because scans always persist to the same cache, so memory == disk. The
  // blanket replace also guarantees that connecting a *different* device
  // flushes stale entries that survived the last disconnect.
  useEffect(() => {
    if (!deviceUid) return;
    let cancelled = false;
    loadSubghzCache(deviceUid).then((cache) => {
      if (cancelled) return;
      setCacheScannedAt(cache?.scannedAt ?? null);
      setEntries(cache?.entries ?? []);
      setFavorites(cache?.favorites ?? []);
    });
    return () => {
      cancelled = true;
    };
  }, [deviceUid, setEntries, setFavorites]);

  const runScan = async () => {
    if (scanning) return;
    setError(null);
    setScanning(true);
    setProgress({ scanned: 0, total: 0, current_path: "" });
    try {
      const effective = await checkBeforeScan(
        [SUBGHZ_ROOT],
        excludedDirs,
        async (next) => {
          await updateSettings({ subghz: { excludedDirs: next } });
        },
      );
      if (effective === null) return; // user closed the prescan modal
      // Feed the current in-memory list (which was hydrated from disk) as
      // the cache hint — Rust skips reading any file whose mtime matches.
      const list = await subghzScan(
        SUBGHZ_ROOT,
        effective,
        entries,
        setProgress,
      );
      setEntries(list);
      if (deviceUid) {
        const cache = await saveSubghzCache(deviceUid, list).catch(() => null);
        if (cache) setFavorites(cache.favorites);
        setCacheScannedAt(Date.now());
      }
      void notify("libraryScan", "Sub-GHz scan complete", `${list.length} entries indexed.`);
    } catch (e) {
      if (!isCommandCancelled(e)) {
        setError(`Scan failed: ${commandErrorMessage(e, "subghz_scan")}`);
      }
    } finally {
      setScanning(false);
      setProgress(null);
    }
  };

  const cancelScan = async () => {
    try {
      await subghzCancelScan();
    } catch {
      /* fine — flag may have already been cleared */
    }
  };

  const protocols = useMemo(
    () => uniqueSorted(entries.map((e) => e.protocol).filter(Boolean) as string[]),
    [entries],
  );

  const favSet = useMemo(() => new Set(favorites), [favorites]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return entries.filter((e) => {
      if (starredOnly && !favSet.has(subghzFavoriteIdentity(e))) return false;
      if (protocolFilter && e.protocol !== protocolFilter) return false;
      if (!q) return true;
      const haystack = [e.name, e.protocol, e.preset, e.key, e.modulation]
        .filter(Boolean)
        .join(" ")
        .toLowerCase();
      return haystack.includes(q);
    });
  }, [entries, query, protocolFilter, starredOnly, favSet]);

  // The library view is reachable while disconnected as long as the cache
  // holds entries — scanning degrades inside the toolbar in that case.

  return (
    <div className="flex-1 min-h-0 flex flex-col overflow-hidden">
      <LibraryToolbar
        protocols={protocols}
        protocolFilter={protocolFilter}
        onProtocolFilterChange={setProtocolFilter}
        query={query}
        onQueryChange={setQuery}
        onRefresh={runScan}
        onCancel={cancelScan}
        total={entries.length}
        filtered={filtered.length}
        lastScannedAt={cacheScannedAt}
        isConnected={isConnected}
        starredOnly={starredOnly}
        onStarredOnlyChange={setStarredOnly}
        favoritesCount={favorites.length}
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
        <EmptyState onScan={runScan} />
      ) : (
        <LibraryTable
          entries={filtered}
          favorites={favSet}
          onToggleFavorite={toggleFavorite}
        />
      )}

      {preScanModal}
    </div>
  );
}

function EmptyState({ onScan }: { onScan: () => void }) {
  return (
    <div className="flex-1 flex flex-col items-center justify-center gap-3 text-dim">
      <RadioTower size={40} strokeWidth={1.5} className="text-elevated" />
      <p className="text-sm">No .sub files indexed yet.</p>
      <button
        onClick={onScan}
        className="px-3 py-1.5 text-xs text-primary bg-accent/20 border border-accent/40 rounded hover:bg-accent/30"
      >
        Scan /ext/subghz
      </button>
    </div>
  );
}

function uniqueSorted(values: string[]): string[] {
  return Array.from(new Set(values)).sort((a, b) => a.localeCompare(b));
}
