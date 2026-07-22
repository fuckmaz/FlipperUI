//! BadUSB / BadKB library scanning.
//!
//! Flipper stores Duckyscript payloads as plain `.txt` files under two roots
//! on firmwares that support both transports:
//!
//! ```text
//! /ext/badusb — USB HID scripts (stock, Momentum, Unleashed, RogueMaster)
//! /ext/badkb  — Bluetooth HID scripts (Momentum / newer forks)
//! ```
//!
//! A script is just text, not a structured keyed file like `.sub` / `.nfc` /
//! `.ir`, so we don't have headers to parse. The library view surfaces the
//! script's *line count* and the first comment line (REM …) as a blurb, and
//! tags each file with the `kind` ("usb" or "kb") inferred from its root.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{FlipperError, Result};
use crate::flipper::client::FlipperClient;
use crate::flipper::library_walk;
use crate::flipper::storage;

/// Parsed metadata for a single BadUSB / BadKB script.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BadUsbEntry {
    #[serde(deserialize_with = "crate::commands::path::deserialize_device_path_string")]
    pub path: String,
    pub name: String,
    /// "usb" for files under `/ext/badusb`, "kb" for `/ext/badkb`.
    pub kind: String,
    /// Non-blank line count (a rough proxy for script length).
    pub line_count: u32,
    /// First comment line (REM … or # …), trimmed. `None` if the script has no
    /// leading comment. Surfaced in the library table as a human blurb.
    pub comment: Option<String>,
    pub size: u32,
    #[serde(default)]
    pub mtime: Option<u32>,
}

/// Recursively scan `roots` for `.txt` Duckyscript files, parse them, and
/// return the combined list. `kind_for_root` maps each root to the entry
/// `kind` label ("usb" or "kb"). Mirrors [`crate::flipper::nfc::scan_library`]
/// — mtime-based cache hits skip re-reads over serial.
pub fn scan_library(
    client: &mut FlipperClient,
    roots: &[(&str, &str)],
    excluded: &[String],
    cached: &HashMap<String, BadUsbEntry>,
    cancelled: &Arc<AtomicBool>,
    on_progress: library_walk::ScanProgress,
) -> Result<Vec<BadUsbEntry>> {
    let mut files: Vec<(String, u32, String)> = Vec::new();

    // Not every firmware has both roots — a missing directory just means no
    // files of that kind, so a listing error on a root is logged and swallowed
    // instead of aborting the whole scan.
    for (root, kind) in roots {
        if let Err(e) = walk_dir(client, root, excluded, kind, &mut files) {
            tracing::warn!(?e, root = %root, "badusb root unavailable — skipping");
        }
    }

    let total = files.len() as u32;
    let mut entries = Vec::with_capacity(files.len());

    for (idx, (path, size, kind)) in files.iter().enumerate() {
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
                hit.kind = kind.clone();
                entries.push(hit);
                continue;
            }
        }

        let bytes = match storage::storage_read(client, path, |_, _| {}, || false) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(?e, %path, "skipping unreadable badusb file");
                continue;
            }
        };

        let text = String::from_utf8_lossy(&bytes);
        let name = library_walk::file_basename(path).to_string();
        let mut entry = parse_script(path, &name, kind, &text);
        entry.size = *size;
        entry.mtime = current_mtime;
        entries.push(entry);
    }

    on_progress(total, total, "");
    Ok(entries)
}

/// Parse a specific list of BadUSB / BadKB `.txt` paths
/// Used after editor saves so one row can be refreshed without a full `/ext/badusb` + `/ext/badkb` scan.
pub fn parse_paths(client: &mut FlipperClient, paths: &[String]) -> Result<Vec<BadUsbEntry>> {
    let mut entries = Vec::with_capacity(paths.len());

    for path in paths {
        if !library_walk::has_extension_ci(path, ".txt") {
            continue;
        }
        let stat = match storage::storage_stat(client, path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(?e, %path, "skipping unstattable badusb path");
                continue;
            }
        };
        if stat.r#type == 1 {
            continue;
        }

        let kind = kind_for_path(path);
        let mtime = storage::storage_timestamp(client, path).ok();
        let bytes = match storage::storage_read(client, path, |_, _| {}, || false) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(?e, %path, "skipping unreadable badusb path");
                continue;
            }
        };

        let text = String::from_utf8_lossy(&bytes);
        let name = library_walk::file_basename(path).to_string();
        let mut entry = parse_script(path, &name, kind, &text);
        entry.size = stat.size;
        entry.mtime = mtime;
        entries.push(entry);
    }

    Ok(entries)
}

fn kind_for_path(path: &str) -> &str {
    if path
        .to_ascii_lowercase()
        .trim_start_matches('/')
        .starts_with("ext/badkb/")
    {
        "kb"
    } else {
        "usb"
    }
}

fn walk_dir(
    client: &mut FlipperClient,
    dir: &str,
    excluded: &[String],
    kind: &str,
    out: &mut Vec<(String, u32, String)>,
) -> Result<()> {
    if library_walk::is_excluded(dir, excluded) {
        return Ok(());
    }
    let files = storage::storage_list(client, dir)?;
    for f in files {
        let child = library_walk::join_path(dir, &f.name)?;
        if f.r#type == 1 {
            walk_dir(client, &child, excluded, kind, out)?;
        } else if library_walk::has_extension_ci(&f.name, ".txt")
            && !library_walk::is_excluded(&child, excluded)
        {
            out.push((child, f.size, kind.to_string()));
        }
    }
    Ok(())
}

/// Parse a Duckyscript body for library-view metadata. Counts non-blank lines
/// and pulls the first `REM …` / `# …` comment as a blurb.
pub fn parse_script(path: &str, name: &str, kind: &str, text: &str) -> BadUsbEntry {
    let mut line_count = 0u32;
    let mut comment: Option<String> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        line_count += 1;

        if comment.is_none() {
            if let Some(rest) = trimmed
                .strip_prefix("REM ")
                .or_else(|| trimmed.strip_prefix("rem "))
            {
                let c = rest.trim();
                if !c.is_empty() {
                    comment = Some(c.to_string());
                }
            } else if let Some(rest) = trimmed.strip_prefix('#') {
                let c = rest.trim();
                if !c.is_empty() {
                    comment = Some(c.to_string());
                }
            }
        }
    }

    BadUsbEntry {
        path: path.to_string(),
        name: name.to_string(),
        kind: kind.to_string(),
        line_count,
        comment,
        size: 0,
        mtime: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_script_with_rem_header() {
        let text = "\
REM Opens a terminal on macOS
GUI SPACE
DELAY 500
STRING terminal
ENTER
";
        let e = parse_script("/ext/badusb/mac.txt", "mac.txt", "usb", text);
        assert_eq!(e.line_count, 5);
        assert_eq!(e.comment.as_deref(), Some("Opens a terminal on macOS"));
        assert_eq!(e.kind, "usb");
    }

    #[test]
    fn parses_script_with_hash_header() {
        let text = "# Windows CMD pop\nGUI r\nDELAY 200\nSTRING cmd\nENTER\n";
        let e = parse_script("/ext/badkb/win.txt", "win.txt", "kb", text);
        assert_eq!(e.comment.as_deref(), Some("Windows CMD pop"));
        assert_eq!(e.kind, "kb");
    }

    #[test]
    fn infers_kind_from_path() {
        assert_eq!(kind_for_path("/ext/badkb/win.txt"), "kb");
        assert_eq!(kind_for_path("/ext/badusb/mac.txt"), "usb");
        assert_eq!(kind_for_path("/any/badusb/mac.txt"), "usb");
    }

    #[test]
    fn blank_lines_do_not_count() {
        let text = "\n\nSTRING hi\n\n\nENTER\n";
        let e = parse_script("/p", "n", "usb", text);
        assert_eq!(e.line_count, 2);
        assert!(e.comment.is_none());
    }

    #[test]
    fn txt_extension_match() {
        assert!(library_walk::has_extension_ci("foo.txt", ".txt"));
        assert!(library_walk::has_extension_ci("foo.TXT", ".txt"));
        assert!(!library_walk::has_extension_ci("foo.md", ".txt"));
        assert!(!library_walk::has_extension_ci("txt", ".txt"));
    }

    #[test]
    fn excluded_path_logic() {
        let excluded = vec!["/ext/badusb/private".to_string()];
        assert!(library_walk::is_excluded("/ext/badusb/private", &excluded));
        assert!(library_walk::is_excluded(
            "/ext/badusb/private/x.txt",
            &excluded
        ));
        assert!(!library_walk::is_excluded(
            "/ext/badusb/public/x.txt",
            &excluded
        ));
    }
}
