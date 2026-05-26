import { useCallback, useState } from "react";
import { LibraryPreScanModal } from "../components/LibraryPreScan/LibraryPreScanModal";
import {
  libraryPrewalk,
  type PrewalkLibrary,
  type PrewalkDirStat,
} from "../lib/tauri";
import { loadSettings } from "../lib/settings";

interface PendingReview {
  flagged: PrewalkDirStat[];
  onConfirm: (newExcluded: string[]) => void;
  onSkip: () => void;
  onCancel: () => void;
}

/**
 * Pre-scan heavy-directory review. Walks `roots` over the device storage
 * before running a real library scan; if any directory crosses the entry
 * count / file size thresholds, opens [LibraryPreScanModal] and lets the
 * user pick which dirs to add to the library's persistent exclusion list.
 *
 * Returns:
 * - The effective excluded list to use for the upcoming scan (existing
 *   exclusions plus any newly checked dirs)
 * - `null` if the user closed the modal — caller should bail out of the scan.
 *
 * When `settings.libraries.preScanReview` is off, the prewalk is skipped and
 * `excludedDirs` is returned unchanged.
 */
export function useLibraryPreScan(library: PrewalkLibrary) {
  const [pending, setPending] = useState<PendingReview | null>(null);

  const checkBeforeScan = useCallback(
    async (
      roots: string[],
      excludedDirs: string[],
      applyExclusions: (next: string[]) => Promise<void>,
    ): Promise<string[] | null> => {
      const settings = await loadSettings();
      if (!settings.libraries.preScanReview) return excludedDirs;

      // If the prewalk fails (e.g. user cancelled, or transport hiccup), let
      // the rejection propagate to the caller rather than silently degrading
      // — matches how the real scan reports failures.
      const flagged = await libraryPrewalk(library, roots, excludedDirs);
      if (flagged.length === 0) return excludedDirs;

      return new Promise<string[] | null>((resolve) => {
        setPending({
          flagged,
          onConfirm: async (newExcluded) => {
            setPending(null);
            const next =
              newExcluded.length === 0
                ? excludedDirs
                : Array.from(new Set([...excludedDirs, ...newExcluded]));
            if (newExcluded.length > 0) {
              try {
                await applyExclusions(next);
              } catch {
                /* settings write failure is non-fatal — still proceed with scan */
              }
            }
            resolve(next);
          },
          onSkip: () => {
            setPending(null);
            resolve(excludedDirs);
          },
          onCancel: () => {
            setPending(null);
            resolve(null);
          },
        });
      });
    },
    [library],
  );

  const modal = pending ? (
    <LibraryPreScanModal
      flagged={pending.flagged}
      onScan={pending.onConfirm}
      onSkip={pending.onSkip}
      onCancel={pending.onCancel}
    />
  ) : null;

  return { checkBeforeScan, modal };
}
