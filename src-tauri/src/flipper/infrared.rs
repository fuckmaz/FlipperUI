//! Infrared library scanning and .ir-file parsing.
//!
//! Walks `/ext/infrared` recursively, reads each `.ir` file via the Storage
//! RPC, and parses Flipper's signal-block text format into a typed
//! [`IrEntry`]. An .ir file is a remote (multiple signals); each signal is
//! either "parsed" (protocol + address + command) or "raw" (frequency +
//! duty_cycle + timing data).
//!
//! Library-view rows represent *files*, not individual signals — a remote
//! is the useful unit in the UI. The per-signal detail lives on `IrEntry`
//! so future detail views can show it without a re-scan.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{FlipperError, Result};
use crate::flipper::client::FlipperClient;
use crate::flipper::library_walk;
use crate::flipper::storage;

/// One signal block from inside a .ir file.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct IrSignal {
    pub name: String,
    /// Either "parsed" or "raw" — mirrors the `type:` key in the file.
    pub kind: String,
    pub protocol: Option<String>,
    pub address: Option<String>,
    pub command: Option<String>,
    pub frequency: Option<u32>,
    pub duty_cycle: Option<f32>,
}

/// Parsed metadata for a single .ir file (a remote).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IrEntry {
    #[serde(deserialize_with = "crate::commands::path::deserialize_device_path_string")]
    pub path: String,
    pub name: String,
    pub signals: Vec<IrSignal>,
    /// File modification time from `storage_timestamp` (epoch seconds).
    #[serde(default)]
    pub mtime: Option<u32>,
}

/// Recursively scan `root` for .ir files, parse them, and return the list.
/// Mirrors [`crate::flipper::subghz::scan_library`] — mtime-based cache
/// hits skip re-reads over serial.
pub fn scan_library(
    client: &mut FlipperClient,
    root: &str,
    excluded: &[String],
    cached: &HashMap<String, IrEntry>,
    cancelled: &Arc<AtomicBool>,
    on_progress: library_walk::ScanProgress,
) -> Result<Vec<IrEntry>> {
    let mut paths: Vec<String> = Vec::new();
    walk_dir(client, root, excluded, &mut paths)?;

    let total = paths.len() as u32;
    let mut entries = Vec::with_capacity(paths.len());

    for (idx, path) in paths.iter().enumerate() {
        if cancelled.load(Ordering::Relaxed) {
            return Err(FlipperError::TransferCancelled);
        }
        on_progress(idx as u32, total, path);

        let current_mtime = storage::storage_timestamp(client, path).ok();
        if let (Some(mtime), Some(cached_entry)) = (current_mtime, cached.get(path)) {
            if cached_entry.mtime == Some(mtime) {
                let mut hit = cached_entry.clone();
                hit.mtime = Some(mtime);
                entries.push(hit);
                continue;
            }
        }

        let bytes = match storage::storage_read(client, path, |_, _| {}, || false) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(?e, %path, "skipping unreadable .ir file");
                continue;
            }
        };

        let text = String::from_utf8_lossy(&bytes);
        let name = library_walk::file_basename(path).to_string();
        let mut entry = parse_ir(path, &name, &text);
        entry.mtime = current_mtime;
        entries.push(entry);
    }

    on_progress(total, total, "");
    Ok(entries)
}

fn walk_dir(
    client: &mut FlipperClient,
    dir: &str,
    excluded: &[String],
    out: &mut Vec<String>,
) -> Result<()> {
    if library_walk::is_excluded(dir, excluded) {
        return Ok(());
    }
    let files = storage::storage_list(client, dir)?;
    for f in files {
        let child = library_walk::join_path(dir, &f.name)?;
        if f.r#type == 1 {
            walk_dir(client, &child, excluded, out)?;
        } else if library_walk::has_extension_ci(&f.name, ".ir")
            && !library_walk::is_excluded(&child, excluded)
        {
            out.push(child);
        }
    }
    Ok(())
}

/// Parse a .ir file's text body. Signals are separated by a `#` line;
/// the header block before the first `#` just carries Filetype/Version
/// and is ignored for library purposes.
pub fn parse_ir(path: &str, name: &str, text: &str) -> IrEntry {
    let mut signals = Vec::new();
    let mut current: Option<IrSignal> = None;

    let flush = |slot: &mut Option<IrSignal>, out: &mut Vec<IrSignal>| {
        if let Some(sig) = slot.take() {
            if !sig.name.is_empty() || !sig.kind.is_empty() {
                out.push(sig);
            }
        }
    };

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            flush(&mut current, &mut signals);
            current = Some(IrSignal::default());
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
        let Some(sig) = current.as_mut() else {
            continue;
        };
        match k {
            "name" => sig.name = v.to_string(),
            "type" => sig.kind = v.to_string(),
            "protocol" => sig.protocol = Some(v.to_string()),
            "address" => sig.address = Some(v.to_string()),
            "command" => sig.command = Some(v.to_string()),
            "frequency" => sig.frequency = v.parse::<u32>().ok(),
            "duty_cycle" => sig.duty_cycle = v.parse::<f32>().ok(),
            _ => {}
        }
    }
    flush(&mut current, &mut signals);

    IrEntry {
        path: path.to_string(),
        name: name.to_string(),
        signals,
        mtime: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remote_with_parsed_and_raw() {
        let text = "\
Filetype: IR signals file
Version: 1
#
name: POWER
type: parsed
protocol: NECext
address: 04 00 00 00
command: 08 00 00 00
#
name: VOL+
type: raw
frequency: 38000
duty_cycle: 0.330000
data: 9042 4502 552 1734
";
        let e = parse_ir("/ext/infrared/tv.ir", "tv.ir", text);
        assert_eq!(e.signals.len(), 2);

        let power = &e.signals[0];
        assert_eq!(power.name, "POWER");
        assert_eq!(power.kind, "parsed");
        assert_eq!(power.protocol.as_deref(), Some("NECext"));
        assert_eq!(power.address.as_deref(), Some("04 00 00 00"));
        assert_eq!(power.command.as_deref(), Some("08 00 00 00"));

        let vol = &e.signals[1];
        assert_eq!(vol.name, "VOL+");
        assert_eq!(vol.kind, "raw");
        assert_eq!(vol.frequency, Some(38_000));
        assert!((vol.duty_cycle.unwrap() - 0.33).abs() < 1e-4);
    }

    #[test]
    fn empty_file_has_no_signals() {
        let e = parse_ir("/p", "n", "Filetype: IR signals file\nVersion: 1\n");
        assert!(e.signals.is_empty());
    }

    #[test]
    fn excluded_path_logic() {
        let excluded = vec!["/ext/infrared/private".to_string()];
        assert!(library_walk::is_excluded(
            "/ext/infrared/private",
            &excluded
        ));
        assert!(library_walk::is_excluded(
            "/ext/infrared/private/x.ir",
            &excluded
        ));
        assert!(!library_walk::is_excluded(
            "/ext/infrared/public/x.ir",
            &excluded
        ));
    }
}
