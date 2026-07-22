import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle } from "lucide-react";
import rfidIconSvg from "../../assets/icons/125.svg";
import { useFlipperStore } from "../../store/useFlipperStore";
import { rfidCancelScan, rfidParsePaths, rfidScan } from "../../lib/tauri";
import { loadSettings, subscribeSettings, updateSettings } from "../../lib/settings";
import { useLibraryPreScan } from "../../hooks/useLibraryPreScan";
import { loadRfidCache, saveRfidCache } from "../../lib/rfidCache";
import { notify } from "../../lib/notify";
import { commandErrorMessage, isCommandCancelled } from "../../lib/commandError";
import { useLibraryDrop } from "../../hooks/useLibraryDrop";
import { LibraryDropOverlay } from "../ui/LibraryDropOverlay";
import { LibraryToolbar } from "./LibraryToolbar";
import { LibraryTable } from "./LibraryTable";

const RFID_ROOT = "/ext/lfrfid";

export function RfidLibrary() {
  const isConnected = useFlipperStore((s) => s.isConnected);
  const deviceUid = useFlipperStore((s) => s.deviceInfo?.hardware_uid ?? null);
  const entries = useFlipperStore((s) => s.rfidEntries);
  const scanning = useFlipperStore((s) => s.rfidScanning);
  const error = useFlipperStore((s) => s.rfidError);
  const setEntries = useFlipperStore((s) => s.setRfidEntries);
  const setScanning = useFlipperStore((s) => s.setRfidScanning);
  const setProgress = useFlipperStore((s) => s.setRfidProgress);
  const setError = useFlipperStore((s) => s.setRfidError);

  const [excludedDirs, setExcludedDirs] = useState<string[]>([]);
  const [query, setQuery] = useState("");
  const [keyTypeFilter, setKeyTypeFilter] = useState<string | null>(null);
  const [cacheScannedAt, setCacheScannedAt] = useState<number | null>(null);
  const { checkBeforeScan, modal: preScanModal } = useLibraryPreScan("rfid");

  useEffect(() => {
    loadSettings().then((s) => setExcludedDirs(s.rfid.excludedDirs));
    return subscribeSettings((s) => setExcludedDirs(s.rfid.excludedDirs));
  }, []);

  const injection = useFlipperStore((s) => s.librarySearchInjection);
  const clearInjection = useFlipperStore((s) => s.setLibrarySearchInjection);
  useEffect(() => {
    if (injection && injection.view === "rfid") {
      setQuery(injection.query);
      setKeyTypeFilter(null);
      clearInjection(null);
    }
  }, [injection, clearInjection]);

  // Rehydrate from disk on every deviceUid change — swaps in the new device's
  // cache (or clears) so entries from a prior device don't linger.
  useEffect(() => {
    if (!deviceUid) return;
    let cancelled = false;
    loadRfidCache(deviceUid).then((cache) => {
      if (cancelled) return;
      setCacheScannedAt(cache?.scannedAt ?? null);
      setEntries(cache?.entries ?? []);
    });
    return () => {
      cancelled = true;
    };
  }, [deviceUid, setEntries]);

  const runScan = useCallback(async () => {
    if (scanning) return;
    setError(null);
    setScanning(true);
    setProgress({ scanned: 0, total: 0, current_path: "" });
    try {
      const effective = await checkBeforeScan(
        [RFID_ROOT],
        excludedDirs,
        async (next) => {
          await updateSettings({ rfid: { excludedDirs: next } });
        },
      );
      if (effective === null) return;
      const list = await rfidScan(RFID_ROOT, effective, entries, setProgress);
      setEntries(list);
      if (deviceUid) {
        await saveRfidCache(deviceUid, list).catch(() => {});
        setCacheScannedAt(Date.now());
      }
      void notify("libraryScan", "RFID scan complete", `${list.length} entries indexed.`);
    } catch (e) {
      if (!isCommandCancelled(e)) {
        setError(`Scan failed: ${commandErrorMessage(e, "rfid_scan")}`);
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
    excludedDirs,
    entries,
    setEntries,
    deviceUid,
    checkBeforeScan,
  ]);

  const cancelScan = async () => {
    try {
      await rfidCancelScan();
    } catch {
      /* fine */
    }
  };

  const mergeUploaded = useCallback(
    async (uploadedPaths: string[]) => {
      if (uploadedPaths.length === 0) return;
      try {
        const parsed = await rfidParsePaths(uploadedPaths);
        if (parsed.length === 0) return;
        const current = useFlipperStore.getState().rfidEntries;
        const byPath = new Map(current.map((e) => [e.path, e]));
        for (const e of parsed) byPath.set(e.path, e);
        const next = Array.from(byPath.values());
        setEntries(next);
        if (deviceUid) {
          await saveRfidCache(deviceUid, next).catch(() => {});
          setCacheScannedAt(Date.now());
        }
      } catch (e) {
        setError(`Couldn't parse uploaded files: ${(e as Error).message || String(e)}`);
      }
    },
    [deviceUid, setEntries, setError],
  );

  const { isDragOver, uploadingCount, openFilePicker, dropZoneHandlers } =
    useLibraryDrop({
      rootPath: RFID_ROOT,
      extensions: ["rfid"],
      isConnected,
      setError,
      onAfterUpload: mergeUploaded,
      kindLabel: "Flipper RFID keys",
    });

  const keyTypes = useMemo(() => {
    const set = new Set<string>();
    for (const e of entries) {
      if (e.key_type) set.add(e.key_type);
    }
    return Array.from(set).sort((a, b) => a.localeCompare(b));
  }, [entries]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return entries.filter((e) => {
      if (keyTypeFilter && e.key_type !== keyTypeFilter) return false;
      if (!q) return true;
      const haystack = [e.name, e.path, e.key_type ?? "", e.data ?? ""]
        .join(" ")
        .toLowerCase();
      return haystack.includes(q);
    });
  }, [entries, query, keyTypeFilter]);

  return (
    <div
      className="flex-1 min-h-0 flex flex-col overflow-hidden relative"
      {...dropZoneHandlers}
    >
      <LibraryToolbar
        keyTypes={keyTypes}
        keyTypeFilter={keyTypeFilter}
        onKeyTypeFilterChange={setKeyTypeFilter}
        query={query}
        onQueryChange={setQuery}
        onRefresh={runScan}
        onCancel={cancelScan}
        onUpload={openFilePicker}
        uploadingCount={uploadingCount}
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

      <LibraryDropOverlay
        visible={isDragOver}
        label={`Drop .rfid files to upload to ${RFID_ROOT}`}
      />

      {preScanModal}
    </div>
  );
}

function EmptyState({ onScan }: { onScan: () => void }) {
  return (
    <div className="flex-1 flex flex-col items-center justify-center gap-3 text-dim">
      <img src={rfidIconSvg} alt="RFID" className="w-10 h-10 text-elevated" />
      <p className="text-sm">No .rfid files indexed yet.</p>
      <button
        onClick={onScan}
        className="px-3 py-1.5 text-xs text-primary bg-accent/20 border border-accent/40 rounded hover:bg-accent/30"
      >
        Scan /ext/lfrfid
      </button>
    </div>
  );
}
