//! 125 kHz RFID library scanning and `.rfid`-file parsing.
//!
//! Walks `/ext/lfrfid` recursively, reads each `.rfid` file via the Storage
//! RPC, and parses Flipper's text key-value header into a typed [`RfidEntry`].
//!
//! Stock firmware serializes `.rfid` files as:
//!
//! ```text
//! Filetype: Flipper RFID key
//! Version: 1
//! Key type: EM4100
//! Data: 12 34 56 78 90
//! ```
//!
//! `Key type` covers EM4100, H10301, Indala26, IDTECK, IoProx, Pyramid39,
//! Awid, Viking, Jablotron, Paradox, PAC/Stanley, Securakey, Gallagher,
//! Nexwatch, Keri, GProxII, and friends.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{FlipperError, Result};
use crate::flipper::client::FlipperClient;
use crate::flipper::library_walk;
use crate::flipper::storage;

/// Parsed metadata for a single `.rfid` file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RfidEntry {
    #[serde(deserialize_with = "crate::commands::path::deserialize_device_path_string")]
    pub path: String,
    pub name: String,
    /// Protocol family reported by stock firmware (e.g. "EM4100", "H10301").
    pub key_type: Option<String>,
    /// Hex payload as written by firmware — kept verbatim for display.
    pub data: Option<String>,
    pub size: u32,
    #[serde(default)]
    pub mtime: Option<u32>,
}

/// Recursively scan `root` for `.rfid` files, parse them, and return the list.
/// Mirrors [`crate::flipper::nfc::scan_library`] — mtime-based cache hits skip
/// re-reads over serial.
pub fn scan_library(
    client: &mut FlipperClient,
    root: &str,
    excluded: &[String],
    cached: &HashMap<String, RfidEntry>,
    cancelled: &Arc<AtomicBool>,
    on_progress: library_walk::ScanProgress,
) -> Result<Vec<RfidEntry>> {
    let mut files: Vec<(String, u32)> = Vec::new();
    walk_dir(client, root, excluded, &mut files)?;

    let total = files.len() as u32;
    let mut entries = Vec::with_capacity(files.len());

    for (idx, (path, size)) in files.iter().enumerate() {
        if cancelled.load(Ordering::Relaxed) {
            return Err(FlipperError::TransferCancelled);
        }
        on_progress(idx as u32, total, path);

        let current_mtime = storage::storage_timestamp(client, path).ok();
        if let (Some(mtime), Some(cached_entry)) = (current_mtime, cached.get(path)) {
            if cached_entry.mtime == Some(mtime) && cached_entry.size == *size {
                let mut hit = cached_entry.clone();
                hit.mtime = Some(mtime);
                hit.size = *size;
                entries.push(hit);
                continue;
            }
        }

        let bytes = match storage::storage_read(client, path, |_, _| {}, || false) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(?e, %path, "skipping unreadable .rfid file");
                continue;
            }
        };

        let text = String::from_utf8_lossy(&bytes);
        let name = library_walk::file_basename(path).to_string();
        let mut entry = parse_rfid(path, &name, &text);
        entry.size = *size;
        entry.mtime = current_mtime;
        entries.push(entry);
    }

    on_progress(total, total, "");
    Ok(entries)
}

/// Parse a specific list of `.rfid` paths without walking any directory.
pub fn parse_paths(client: &mut FlipperClient, paths: &[String]) -> Result<Vec<RfidEntry>> {
    let mut entries = Vec::with_capacity(paths.len());

    for path in paths {
        if !library_walk::has_extension_ci(path, ".rfid") {
            continue;
        }
        let stat = match storage::storage_stat(client, path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(?e, %path, "skipping unstattable .rfid path");
                continue;
            }
        };
        if stat.r#type == 1 {
            continue;
        }

        let mtime = storage::storage_timestamp(client, path).ok();
        let bytes = match storage::storage_read(client, path, |_, _| {}, || false) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(?e, %path, "skipping unreadable .rfid path");
                continue;
            }
        };

        let text = String::from_utf8_lossy(&bytes);
        let name = library_walk::file_basename(path).to_string();
        let mut entry = parse_rfid(path, &name, &text);
        entry.size = stat.size;
        entry.mtime = mtime;
        entries.push(entry);
    }

    Ok(entries)
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
    let files = storage::storage_list(client, dir)?;
    for f in files {
        let child = library_walk::join_path(dir, &f.name)?;
        if f.r#type == 1 {
            walk_dir(client, &child, excluded, out)?;
        } else if library_walk::has_extension_ci(&f.name, ".rfid")
            && !library_walk::is_excluded(&child, excluded)
        {
            out.push((child, f.size));
        }
    }
    Ok(())
}

/// Parse an `.rfid` text header. Stops once we've seen the keys we care
/// about — the file is small but staying minimal mirrors the NFC parser.
pub fn parse_rfid(path: &str, name: &str, text: &str) -> RfidEntry {
    let mut entry = RfidEntry {
        path: path.to_string(),
        name: name.to_string(),
        key_type: None,
        data: None,
        size: 0,
        mtime: None,
    };

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((k, v)) = trimmed.split_once(':') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim();
        if v.is_empty() {
            continue;
        }

        let kl = k.to_ascii_lowercase();
        match kl.as_str() {
            "key type" => entry.key_type = Some(v.to_string()),
            "data" => entry.data = Some(v.to_string()),
            _ => {}
        }

        if entry.key_type.is_some() && entry.data.is_some() {
            break;
        }
    }

    entry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_em4100_header() {
        let text = "\
Filetype: Flipper RFID key
Version: 1
Key type: EM4100
Data: 12 34 56 78 90
";
        let e = parse_rfid("/ext/lfrfid/card.rfid", "card.rfid", text);
        assert_eq!(e.key_type.as_deref(), Some("EM4100"));
        assert_eq!(e.data.as_deref(), Some("12 34 56 78 90"));
    }

    #[test]
    fn parses_h10301_header() {
        let text = "Filetype: Flipper RFID key\nKey type: H10301\nData: AB CD EF\n";
        let e = parse_rfid("/p", "n", text);
        assert_eq!(e.key_type.as_deref(), Some("H10301"));
        assert_eq!(e.data.as_deref(), Some("AB CD EF"));
    }

    #[test]
    fn missing_fields_are_none() {
        let text = "Filetype: Flipper RFID key\nVersion: 1\n";
        let e = parse_rfid("/p", "n", text);
        assert!(e.key_type.is_none());
        assert!(e.data.is_none());
    }

    #[test]
    fn case_insensitive_keys() {
        let text = "key type: indala26\nDATA: 11 22\n";
        let e = parse_rfid("/p", "n", text);
        assert_eq!(e.key_type.as_deref(), Some("indala26"));
        assert_eq!(e.data.as_deref(), Some("11 22"));
    }

    #[test]
    fn rfid_extension_match() {
        assert!(library_walk::has_extension_ci("foo.rfid", ".rfid"));
        assert!(library_walk::has_extension_ci("foo.RFID", ".rfid"));
        assert!(!library_walk::has_extension_ci("foo.rfi", ".rfid"));
        assert!(!library_walk::has_extension_ci("rfid", ".rfid"));
        assert!(library_walk::has_extension_ci("a.rfid", ".rfid"));
    }

    #[test]
    fn excluded_path_logic() {
        let excluded = vec!["/ext/lfrfid/private".to_string()];
        assert!(library_walk::is_excluded("/ext/lfrfid/private", &excluded));
        assert!(library_walk::is_excluded(
            "/ext/lfrfid/private/x.rfid",
            &excluded
        ));
        assert!(!library_walk::is_excluded(
            "/ext/lfrfid/public/x.rfid",
            &excluded
        ));
    }
}
