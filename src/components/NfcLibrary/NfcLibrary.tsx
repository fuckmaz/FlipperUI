import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle, Nfc } from "lucide-react";
import { useFlipperStore } from "../../store/useFlipperStore";
import { nfcCancelScan, nfcParsePaths, nfcScan } from "../../lib/tauri";
import { loadSettings, subscribeSettings, updateSettings } from "../../lib/settings";
import { useLibraryPreScan } from "../../hooks/useLibraryPreScan";
import { loadNfcCache, saveNfcCache } from "../../lib/nfcCache";
import { notify } from "../../lib/notify";
import { commandErrorMessage, isCommandCancelled } from "../../lib/commandError";
import { useLibraryDrop } from "../../hooks/useLibraryDrop";
import { LibraryDropOverlay } from "../ui/LibraryDropOverlay";
import { LibraryToolbar } from "./LibraryToolbar";
import { LibraryTable } from "./LibraryTable";

const NFC_ROOT = "/ext/nfc";

export function NfcLibrary() {
  const isConnected = useFlipperStore((s) => s.isConnected);
  const deviceUid = useFlipperStore((s) => s.deviceInfo?.hardware_uid ?? null);
  const entries = useFlipperStore((s) => s.nfcEntries);
  const scanning = useFlipperStore((s) => s.nfcScanning);
  const error = useFlipperStore((s) => s.nfcError);
  const setEntries = useFlipperStore((s) => s.setNfcEntries);
  const setScanning = useFlipperStore((s) => s.setNfcScanning);
  const setProgress = useFlipperStore((s) => s.setNfcProgress);
  const setError = useFlipperStore((s) => s.setNfcError);

  const [excludedDirs, setExcludedDirs] = useState<string[]>([]);
  const [query, setQuery] = useState("");
  const [deviceTypeFilter, setDeviceTypeFilter] = useState<string | null>(null);
  const [cacheScannedAt, setCacheScannedAt] = useState<number | null>(null);
  const { checkBeforeScan, modal: preScanModal } = useLibraryPreScan("nfc");

  useEffect(() => {
    loadSettings().then((s) => setExcludedDirs(s.nfc.excludedDirs));
    return subscribeSettings((s) => setExcludedDirs(s.nfc.excludedDirs));
  }, []);

  // Pull in any pending GlobalSearch query targeted at this view, then clear
  // it so the next mount / next search doesn't re-apply a stale filter.
  // Subscribed (rather than mount-only) so re-selecting a result while already
  // on this view still applies the new query.
  const injection = useFlipperStore((s) => s.librarySearchInjection);
  const clearInjection = useFlipperStore((s) => s.setLibrarySearchInjection);
  useEffect(() => {
    if (injection && injection.view === "nfc") {
      setQuery(injection.query);
      setDeviceTypeFilter(null);
      clearInjection(null);
    }
  }, [injection, clearInjection]);

  // Rehydrate from disk on every deviceUid change — swaps in the new device's
  // cache (or clears, if it has never been scanned) so entries from a prior
  // device don't linger after reconnect to a different one.
  useEffect(() => {
    if (!deviceUid) return;
    let cancelled = false;
    loadNfcCache(deviceUid).then((cache) => {
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
        [NFC_ROOT],
        excludedDirs,
        async (next) => {
          await updateSettings({ nfc: { excludedDirs: next } });
        },
      );
      if (effective === null) return; // user closed the prescan modal
      const list = await nfcScan(NFC_ROOT, effective, entries, setProgress);
      setEntries(list);
      if (deviceUid) {
        await saveNfcCache(deviceUid, list).catch(() => {});
        setCacheScannedAt(Date.now());
      }
      void notify("libraryScan", "NFC scan complete", `${list.length} entries indexed.`);
    } catch (e) {
      if (!isCommandCancelled(e)) {
        setError(`Scan failed: ${commandErrorMessage(e, "nfc_scan")}`);
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
      await nfcCancelScan();
    } catch {
      /* fine */
    }
  };

  // Incremental merge after upload: parse only the freshly-written paths and
  // splice them into the existing entries (replacing same-path overwrites).
  // Avoids re-walking /ext/nfc just to surface newly-dropped cards.
  const mergeUploaded = useCallback(
    async (uploadedPaths: string[]) => {
      if (uploadedPaths.length === 0) return;
      try {
        const parsed = await nfcParsePaths(uploadedPaths);
        if (parsed.length === 0) return;
        const current = useFlipperStore.getState().nfcEntries;
        const byPath = new Map(current.map((e) => [e.path, e]));
        for (const e of parsed) byPath.set(e.path, e);
        const next = Array.from(byPath.values());
        setEntries(next);
        if (deviceUid) {
          await saveNfcCache(deviceUid, next).catch(() => {});
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
      rootPath: NFC_ROOT,
      extensions: ["nfc"],
      isConnected,
      setError,
      onAfterUpload: mergeUploaded,
      kindLabel: "Flipper NFC cards",
    });

  const deviceTypes = useMemo(() => {
    const set = new Set<string>();
    for (const e of entries) {
      if (e.device_type) set.add(e.device_type);
    }
    return Array.from(set).sort((a, b) => a.localeCompare(b));
  }, [entries]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return entries.filter((e) => {
      if (deviceTypeFilter && e.device_type !== deviceTypeFilter) return false;
      if (!q) return true;
      const haystack = [
        e.name,
        e.path,
        e.device_type ?? "",
        e.uid ?? "",
        e.mifare_type ?? "",
      ]
        .join(" ")
        .toLowerCase();
      return haystack.includes(q);
    });
  }, [entries, query, deviceTypeFilter]);

  // Browsable while disconnected — scan, upload, and drag-drop degrade below.

  return (
    <div
      className="flex-1 min-h-0 flex flex-col overflow-hidden relative"
      {...dropZoneHandlers}
    >
      <LibraryToolbar
        deviceTypes={deviceTypes}
        deviceTypeFilter={deviceTypeFilter}
        onDeviceTypeFilterChange={setDeviceTypeFilter}
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
        label={`Drop .nfc files to upload to ${NFC_ROOT}`}
      />

      {preScanModal}
    </div>
  );
}

function EmptyState({ onScan }: { onScan: () => void }) {
  return (
    <div className="flex-1 flex flex-col items-center justify-center gap-3 text-dim">
      <Nfc size={40} strokeWidth={1.5} className="text-elevated" />
      <p className="text-sm">No .nfc files indexed yet.</p>
      <button
        onClick={onScan}
        className="px-3 py-1.5 text-xs text-primary bg-accent/20 border border-accent/40 rounded hover:bg-accent/30"
      >
        Scan /ext/nfc
      </button>
    </div>
  );
}
