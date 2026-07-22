import { lazy, Suspense, useEffect, useMemo, useState } from "react";
import { AlertTriangle, Usb } from "lucide-react";
import { useFlipperStore } from "../../store/useFlipperStore";
import { badusbCancelScan, badusbScan } from "../../lib/tauri";
import { loadSettings, subscribeSettings, updateSettings } from "../../lib/settings";
import { useLibraryPreScan } from "../../hooks/useLibraryPreScan";
import { loadBadUsbCache, saveBadUsbCache } from "../../lib/badusbCache";
import { notify } from "../../lib/notify";
import { commandErrorMessage, isCommandCancelled } from "../../lib/commandError";
import { LibraryToolbar } from "./LibraryToolbar";
import { LibraryTable } from "./LibraryTable";
import { Spinner } from "../ui/Spinner";
import type { BadUsbEntry } from "../../types/badusb";

const USB_ROOT = "/ext/badusb";
const KB_ROOT = "/ext/badkb";
const BadUsbEditorModal = lazy(() =>
  import("./BadUsbEditorModal").then((m) => ({ default: m.BadUsbEditorModal })),
);

export function BadUsbLibrary() {
  const isConnected = useFlipperStore((s) => s.isConnected);
  const deviceUid = useFlipperStore((s) => s.deviceInfo?.hardware_uid ?? null);
  const entries = useFlipperStore((s) => s.badusbEntries);
  const scanning = useFlipperStore((s) => s.badusbScanning);
  const error = useFlipperStore((s) => s.badusbError);
  const setEntries = useFlipperStore((s) => s.setBadUsbEntries);
  const setScanning = useFlipperStore((s) => s.setBadUsbScanning);
  const setProgress = useFlipperStore((s) => s.setBadUsbProgress);
  const setError = useFlipperStore((s) => s.setBadUsbError);

  const [excludedDirs, setExcludedDirs] = useState<string[]>([]);
  const [query, setQuery] = useState("");
  const [kindFilter, setKindFilter] = useState<string | null>(null);
  const [cacheScannedAt, setCacheScannedAt] = useState<number | null>(null);
  const [previewEntry, setPreviewEntry] = useState<BadUsbEntry | null>(null);
  const { checkBeforeScan, modal: preScanModal } = useLibraryPreScan("badusb");

  useEffect(() => {
    loadSettings().then((s) => setExcludedDirs(s.badusb.excludedDirs));
    return subscribeSettings((s) => setExcludedDirs(s.badusb.excludedDirs));
  }, []);

  const injection = useFlipperStore((s) => s.librarySearchInjection);
  const clearInjection = useFlipperStore((s) => s.setLibrarySearchInjection);
  useEffect(() => {
    if (injection && injection.view === "badusb") {
      setQuery(injection.query);
      setKindFilter(null);
      clearInjection(null);
    }
  }, [injection, clearInjection]);

  // Rehydrate from disk on every deviceUid change
  useEffect(() => {
    if (!deviceUid) return;
    let cancelled = false;
    loadBadUsbCache(deviceUid).then((cache) => {
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
        [USB_ROOT, KB_ROOT],
        excludedDirs,
        async (next) => {
          await updateSettings({ badusb: { excludedDirs: next } });
        },
      );
      if (effective === null) return;
      const list = await badusbScan(
        USB_ROOT,
        KB_ROOT,
        effective,
        entries,
        setProgress,
      );
      setEntries(list);
      if (deviceUid) {
        await saveBadUsbCache(deviceUid, list).catch(() => {});
        setCacheScannedAt(Date.now());
      }
      void notify("libraryScan", "BadUSB scan complete", `${list.length} entries indexed.`);
    } catch (e) {
      if (!isCommandCancelled(e)) {
        setError(`Scan failed: ${commandErrorMessage(e, "badusb_scan")}`);
      }
    } finally {
      setScanning(false);
      setProgress(null);
    }
  };

  const cancelScan = async () => {
    try {
      await badusbCancelScan();
    } catch {
      /* fine — flag may have already been cleared */
    }
  };

  const onEditorSaved = async (updated: BadUsbEntry) => {
    const next = entries.map((e) => (e.path === updated.path ? updated : e));
    setEntries(next);
    if (deviceUid) await saveBadUsbCache(deviceUid, next).catch(() => {});
  };

  const kinds = useMemo(() => {
    const set = new Set<string>();
    for (const e of entries) set.add(e.kind);
    return Array.from(set).sort();
  }, [entries]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return entries.filter((e) => {
      if (kindFilter && e.kind !== kindFilter) return false;
      if (!q) return true;
      const haystack = [e.name, e.path, e.comment ?? "", e.kind]
        .join(" ")
        .toLowerCase();
      return haystack.includes(q);
    });
  }, [entries, query, kindFilter]);

  // Browsable while disconnected — scan/preview degrade below.

  return (
    <div className="flex-1 min-h-0 flex flex-col overflow-hidden relative">
      <LibraryToolbar
        kinds={kinds}
        kindFilter={kindFilter}
        onKindFilterChange={setKindFilter}
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
        <LibraryTable
          entries={filtered}
          allEntries={entries}
          onPreview={(entry) => {
            if (!isConnected) {
              setError("Connect a Flipper to edit script contents.");
              return;
            }
            setPreviewEntry(entry);
          }}
        />
      )}

      {previewEntry && (
        <Suspense fallback={<EditorLoadingOverlay />}>
          <BadUsbEditorModal
            entry={previewEntry}
            onClose={() => setPreviewEntry(null)}
            onSaved={onEditorSaved}
          />
        </Suspense>
      )}

      {preScanModal}
    </div>
  );
}

function EditorLoadingOverlay() {
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-app/70 backdrop-blur-sm">
      <Spinner size={20} />
    </div>
  );
}

function EmptyState({ onScan }: { onScan: () => void }) {
  return (
    <div className="flex-1 flex flex-col items-center justify-center gap-3 text-dim">
      <Usb size={40} strokeWidth={1.5} className="text-elevated" />
      <p className="text-sm">No BadUSB / BadKB scripts indexed yet.</p>
      <button
        onClick={onScan}
        className="px-3 py-1.5 text-xs text-primary bg-accent/20 border border-accent/40 rounded hover:bg-accent/30"
      >
        Scan /ext/badusb + /ext/badkb
      </button>
    </div>
  );
}
