//! Application library scanning.
//!
//! Walks one or more roots (default `/ext/apps`, plus any user-configured
//! extra dirs) recursively, collects `.fap` files, and returns lightweight
//! [`AppEntry`] records. Unlike the SubGhz / IR scans we don't read the
//! file body — metadata parsing from the ELF-embedded FAP manifest would
//! be a separate, heavier pass. Filename + parent-dir + size is enough to
//! drive the library table today.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{FlipperError, Result};
use crate::flipper::client::FlipperClient;
use crate::flipper::library_walk;
use crate::flipper::storage;

/// Metadata for a single `.fap` on the device.
///
/// `category` is the name of the immediate parent directory (e.g.
/// `/ext/apps/Tools/foo.fap` → `Some("Tools")`). Apps sitting loose at
/// `/ext/apps/foo.fap` come back with `None` — the UI can show those
/// under a synthetic "Uncategorized" bucket.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppEntry {
    #[serde(deserialize_with = "crate::commands::path::deserialize_device_path_string")]
    pub path: String,
    /// Filename without the `.fap` suffix.
    pub name: String,
    pub category: Option<String>,
    pub size: u32,
    /// Parent root the scan discovered this app under (e.g. `/ext/apps`).
    #[serde(deserialize_with = "crate::commands::path::deserialize_device_path_string")]
    pub root: String,
    #[serde(default)]
    pub mtime: Option<u32>,
}

/// Scan a set of roots for `.fap` files and return a deduped list.
///
/// Dedupe key is the full path — if the same file appears via two
/// overlapping roots (e.g. the user added `/ext/apps/Tools` as an extra
/// dir when `/ext/apps` is already scanned), we keep the first discovery.
pub fn scan_library(
    client: &mut FlipperClient,
    roots: &[String],
    excluded: &[String],
    cached: &HashMap<String, AppEntry>,
    cancelled: &Arc<AtomicBool>,
    on_progress: library_walk::ScanProgress,
) -> Result<Vec<AppEntry>> {
    // Phase 1: walk every root, collecting (path, size, root). We collect
    // first so `total` in progress events is meaningful.
    let mut found: Vec<(String, u32, String)> = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        let root = root.trim_end_matches('/').to_string();
        if root.is_empty() {
            continue;
        }
        let mut local: Vec<(String, u32)> = Vec::new();
        walk_dir(client, &root, excluded, &mut local)?;
        for (path, size) in local {
            if seen.insert(path.clone()) {
                found.push((path, size, root.clone()));
            }
        }
    }

    let total = found.len() as u32;
    let mut entries = Vec::with_capacity(found.len());

    for (idx, (path, size, root)) in found.into_iter().enumerate() {
        if cancelled.load(Ordering::Relaxed) {
            return Err(FlipperError::TransferCancelled);
        }
        on_progress(idx as u32, total, &path);

        // Cheap cache hit: mtime + size unchanged means we reuse the cached
        // entry. We still storage_timestamp the file since the device's
        // size can match while mtime bumps (tooling-written .fap).
        let current_mtime = storage::storage_timestamp(client, &path).ok();
        if let Some(cached_entry) = cached.get(&path) {
            let size_matches = cached_entry.size == size;
            let mtime_matches = matches!(current_mtime, Some(t) if cached_entry.mtime == Some(t));
            if size_matches && mtime_matches {
                let mut hit = cached_entry.clone();
                hit.mtime = current_mtime;
                hit.size = size;
                hit.root = root;
                entries.push(hit);
                continue;
            }
        }

        let name = library_walk::file_basename(&path)
            .strip_suffix_ignore_case(".fap")
            .unwrap_or_else(|| library_walk::file_basename(&path).to_string());
        let category = parent_dir_name(&path, &root);

        entries.push(AppEntry {
            path: path.clone(),
            name,
            category,
            size,
            root,
            mtime: current_mtime,
        });
    }

    on_progress(total, total, "");
    Ok(entries)
}

/// Parse a specific list of `.fap` paths without walking any directory.
///
/// Used by the upload-completion path so freshly-installed apps can be merged
/// into the library view without re-walking the apps roots. The matching root
/// is picked as the longest path-prefix from `roots` that contains the file;
/// paths that match no root are dropped.
pub fn parse_paths(
    client: &mut FlipperClient,
    paths: &[String],
    roots: &[String],
) -> Result<Vec<AppEntry>> {
    let normalized_roots: Vec<String> = roots
        .iter()
        .map(|r| r.trim_end_matches('/').to_string())
        .filter(|r| !r.is_empty())
        .collect();

    let mut entries = Vec::with_capacity(paths.len());

    for path in paths {
        if !library_walk::has_extension_ci(path, ".fap") {
            continue;
        }
        let stat = match storage::storage_stat(client, path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(?e, %path, "skipping unstattable .fap path");
                continue;
            }
        };
        if stat.r#type == 1 {
            continue;
        }

        let Some(root) = pick_root(path, &normalized_roots) else {
            tracing::warn!(%path, "no matching apps root, skipping");
            continue;
        };

        let mtime = storage::storage_timestamp(client, path).ok();
        let name = library_walk::file_basename(path)
            .strip_suffix_ignore_case(".fap")
            .unwrap_or_else(|| library_walk::file_basename(path).to_string());
        let category = parent_dir_name(path, &root);

        entries.push(AppEntry {
            path: path.clone(),
            name,
            category,
            size: stat.size,
            root,
            mtime,
        });
    }

    Ok(entries)
}

fn pick_root(path: &str, roots: &[String]) -> Option<String> {
    roots
        .iter()
        .filter(|r| path == r.as_str() || path.starts_with(&format!("{r}/")))
        .max_by_key(|r| r.len())
        .cloned()
}

fn walk_dir(
    client: &mut FlipperClient,
    dir: &str,
    excluded: &[String],
    out: &mut Vec<(String, u32)>,
) -> Result<()> {
    if library_walk::is_excluded(dir, excluded) {
        return Ok(());
    }
    // A missing root (user added a nonexistent extra dir) should be
    // silently skipped rather than aborting the whole scan.
    let files = match storage::storage_list(client, dir) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(?e, %dir, "skipping unreadable apps dir");
            return Ok(());
        }
    };
    for f in files {
        let child = library_walk::join_path(dir, &f.name)?;
        if f.r#type == 1 {
            walk_dir(client, &child, excluded, out)?;
        } else if library_walk::has_extension_ci(&f.name, ".fap")
            && !library_walk::is_excluded(&child, excluded)
        {
            out.push((child, f.size));
        }
    }
    Ok(())
}

/// The immediate parent directory name, unless the parent *is* the root.
/// `/ext/apps/Tools/foo.fap` under root `/ext/apps` → `Some("Tools")`
/// `/ext/apps/foo.fap`       under root `/ext/apps` → `None`
fn parent_dir_name(path: &str, root: &str) -> Option<String> {
    let root = root.trim_end_matches('/');
    let parent = path.rsplit_once('/').map(|(p, _)| p)?;
    if parent == root {
        return None;
    }
    parent.rsplit_once('/').map(|(_, n)| n.to_string())
}

/// Small helper to avoid pulling in `heck` or writing ad-hoc slicing inline.
trait StripSuffixIgnoreCase {
    fn strip_suffix_ignore_case(&self, suffix: &str) -> Option<String>;
}
impl StripSuffixIgnoreCase for str {
    fn strip_suffix_ignore_case(&self, suffix: &str) -> Option<String> {
        if self.len() < suffix.len() {
            return None;
        }
        let (head, tail) = self.split_at(self.len() - suffix.len());
        if tail.eq_ignore_ascii_case(suffix) {
            Some(head.to_string())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_dir_name_unnested() {
        assert_eq!(parent_dir_name("/ext/apps/foo.fap", "/ext/apps"), None);
    }

    #[test]
    fn parent_dir_name_nested() {
        assert_eq!(
            parent_dir_name("/ext/apps/Tools/foo.fap", "/ext/apps"),
            Some("Tools".into())
        );
    }

    #[test]
    fn parent_dir_name_deeply_nested() {
        assert_eq!(
            parent_dir_name("/ext/apps/Games/Chess/chess.fap", "/ext/apps"),
            Some("Chess".into())
        );
    }

    #[test]
    fn strip_suffix_ignore_case_works() {
        assert_eq!(
            "foo.FAP".strip_suffix_ignore_case(".fap"),
            Some("foo".into())
        );
        assert_eq!(
            "foo.fap".strip_suffix_ignore_case(".FAP"),
            Some("foo".into())
        );
        assert_eq!("foo.txt".strip_suffix_ignore_case(".fap"), None);
    }

    #[test]
    fn exclusion_logic() {
        let excluded = vec!["/ext/apps/Debug".to_string()];
        assert!(library_walk::is_excluded("/ext/apps/Debug", &excluded));
        assert!(library_walk::is_excluded(
            "/ext/apps/Debug/x.fap",
            &excluded
        ));
        assert!(!library_walk::is_excluded("/ext/apps/Debugger", &excluded));
        assert!(!library_walk::is_excluded(
            "/ext/apps/Tools/x.fap",
            &excluded
        ));
    }

    #[test]
    fn fap_extension_match() {
        assert!(library_walk::has_extension_ci("foo.fap", ".fap"));
        assert!(library_walk::has_extension_ci("foo.FAP", ".fap"));
        assert!(!library_walk::has_extension_ci("foo.txt", ".fap"));
        assert!(!library_walk::has_extension_ci("fap", ".fap"));
        assert!(library_walk::has_extension_ci("a.fap", ".fap"));
    }
}
