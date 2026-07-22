import { useEffect, useMemo, useState } from "react";
import { Tv, AlertTriangle } from "lucide-react";
import { useFlipperStore } from "../../store/useFlipperStore";
import { infraredCancelScan, infraredScan } from "../../lib/tauri";
import { loadSettings, subscribeSettings, updateSettings } from "../../lib/settings";
import { useLibraryPreScan } from "../../hooks/useLibraryPreScan";
import { loadInfraredCache, saveInfraredCache } from "../../lib/infraredCache";
import { notify } from "../../lib/notify";
import { commandErrorMessage, isCommandCancelled } from "../../lib/commandError";
import { LibraryToolbar } from "./LibraryToolbar";
import { LibraryTable } from "./LibraryTable";

const IR_ROOT = "/ext/infrared";

export function InfraredLibrary() {
  const isConnected = useFlipperStore((s) => s.isConnected);
  const deviceUid = useFlipperStore((s) => s.deviceInfo?.hardware_uid ?? null);
  const entries = useFlipperStore((s) => s.irEntries);
  const scanning = useFlipperStore((s) => s.irScanning);
  const error = useFlipperStore((s) => s.irError);
  const setEntries = useFlipperStore((s) => s.setIrEntries);
  const setScanning = useFlipperStore((s) => s.setIrScanning);
  const setProgress = useFlipperStore((s) => s.setIrProgress);
  const setError = useFlipperStore((s) => s.setIrError);

  const [excludedDirs, setExcludedDirs] = useState<string[]>([]);
  const [query, setQuery] = useState("");
  const [protocolFilter, setProtocolFilter] = useState<string | null>(null);
  const [cacheScannedAt, setCacheScannedAt] = useState<number | null>(null);
  const { checkBeforeScan, modal: preScanModal } = useLibraryPreScan("infrared");

  useEffect(() => {
    loadSettings().then((s) => setExcludedDirs(s.infrared.excludedDirs));
    return subscribeSettings((s) => setExcludedDirs(s.infrared.excludedDirs));
  }, []);

  const injection = useFlipperStore((s) => s.librarySearchInjection);
  const clearInjection = useFlipperStore((s) => s.setLibrarySearchInjection);
  useEffect(() => {
    if (injection && injection.view === "infrared") {
      setQuery(injection.query);
      setProtocolFilter(null);
      clearInjection(null);
    }
  }, [injection, clearInjection]);

  // Rehydrate from disk on every deviceUid change; clears stale entries from
  // a previous device and makes the library available as soon as a device is
  // known. Disk is the source of truth — scans always save before returning.
  useEffect(() => {
    if (!deviceUid) return;
    let cancelled = false;
    loadInfraredCache(deviceUid).then((cache) => {
      if (cancelled) return;
      setCacheScannedAt(cache?.scannedAt ?? null);
      setEntries(cache?.entries ?? []);
    });
    return () => {
      cancelled = true;
    };
  }, [deviceUid, setEntries]);

  const runScan = async () => {
    if (scanning) return;
    setError(null);
    setScanning(true);
    setProgress({ scanned: 0, total: 0, current_path: "" });
    try {
      const effective = await checkBeforeScan(
        [IR_ROOT],
        excludedDirs,
        async (next) => {
          await updateSettings({ infrared: { excludedDirs: next } });
        },
      );
      if (effective === null) return;
      const list = await infraredScan(IR_ROOT, effective, entries, setProgress);
      setEntries(list);
      if (deviceUid) {
        await saveInfraredCache(deviceUid, list).catch(() => {});
        setCacheScannedAt(Date.now());
      }
      void notify("libraryScan", "Infrared scan complete", `${list.length} entries indexed.`);
    } catch (e) {
      if (!isCommandCancelled(e)) {
        setError(`Scan failed: ${commandErrorMessage(e, "infrared_scan")}`);
      }
    } finally {
      setScanning(false);
      setProgress(null);
    }
  };

  const cancelScan = async () => {
    try {
      await infraredCancelScan();
    } catch {
      /* fine — flag may have already been cleared */
    }
  };

  const protocols = useMemo(() => {
    const set = new Set<string>();
    for (const e of entries) {
      for (const s of e.signals) {
        if (s.protocol) set.add(s.protocol);
      }
    }
    return Array.from(set).sort((a, b) => a.localeCompare(b));
  }, [entries]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return entries.filter((e) => {
      if (protocolFilter) {
        if (!e.signals.some((s) => s.protocol === protocolFilter)) return false;
      }
      if (!q) return true;
      const haystack = [
        e.name,
        e.path,
        ...e.signals.flatMap((s) => [s.name, s.protocol, s.address, s.command]),
      ]
        .filter(Boolean)
        .join(" ")
        .toLowerCase();
      return haystack.includes(q);
    });
  }, [entries, query, protocolFilter]);

  // Browsable while disconnected — scanning degrades in the toolbar.

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
        <LibraryTable entries={filtered} />
      )}

      {preScanModal}
    </div>
  );
}

function EmptyState({ onScan }: { onScan: () => void }) {
  return (
    <div className="flex-1 flex flex-col items-center justify-center gap-3 text-dim">
      <Tv size={40} strokeWidth={1.5} className="text-elevated" />
      <p className="text-sm">No .ir files indexed yet.</p>
      <button
        onClick={onScan}
        className="px-3 py-1.5 text-xs text-primary bg-accent/20 border border-accent/40 rounded hover:bg-accent/30"
      >
        Scan /ext/infrared
      </button>
    </div>
  );
}
