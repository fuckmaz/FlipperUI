//! Sub-GHz library scanning and .sub-file parsing.
//!
//! Walks `/ext/subghz` (or any root) recursively, reads each `.sub` file via
//! the Storage RPC, and parses Flipper's simple `Key: Value` text format into
//! a typed [`SubGhzEntry`]. Excluded paths are skipped during the walk so we
//! never hit the device for files the user doesn't want indexed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{FlipperError, Result};
use crate::flipper::client::FlipperClient;
use crate::flipper::library_walk;
use crate::flipper::storage;

/// Coordinates extracted from a .sub file (manual annotations or capture-with-GPS plugins).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Coordinates {
    pub lat: f64,
    pub lon: f64,
}

/// Parsed metadata for a single .sub file. All header fields are optional —
/// RAW captures don't have Bit/Key/TE; some captures omit Protocol or Preset.
/// `mtime` is set by the scan after a successful `storage_timestamp` — the
/// frontend uses it to invalidate cache entries on re-scan.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubGhzEntry {
    #[serde(deserialize_with = "crate::commands::path::deserialize_device_path_string")]
    pub path: String,
    pub name: String,
    pub frequency: Option<u32>,
    pub preset: Option<String>,
    pub protocol: Option<String>,
    pub bit: Option<u32>,
    pub te: Option<u32>,
    pub key: Option<String>,
    /// Derived from the preset string (OOK / FM / unknown).
    pub modulation: Option<String>,
    pub coordinates: Option<Coordinates>,
    /// Whether the file contains a RAW_Data section (full waveform capture).
    pub has_raw: bool,
    /// File modification time from `storage_timestamp` (epoch seconds).
    #[serde(default)]
    pub mtime: Option<u32>,
}

/// Recursively scan `root` for .sub files, parse them, and return the list.
///
/// `excluded` are absolute paths under `root` to skip entirely. `cached` is a
/// map of previously-parsed entries keyed by absolute path; for each path we
/// re-discover on disk, we check `storage_timestamp` and reuse the cached
/// entry when the mtime matches — avoiding a full `storage_read` round-trip.
/// `cancelled` is checked between files so the caller can abort cleanly.
pub fn scan_library(
    client: &mut FlipperClient,
    root: &str,
    excluded: &[String],
    cached: &HashMap<String, SubGhzEntry>,
    cancelled: &Arc<AtomicBool>,
    on_progress: library_walk::ScanProgress,
) -> Result<Vec<SubGhzEntry>> {
    let mut paths: Vec<String> = Vec::new();
    walk_dir(client, root, excluded, &mut paths)?;

    let total = paths.len() as u32;
    let mut entries = Vec::with_capacity(paths.len());

    for (idx, path) in paths.iter().enumerate() {
        if cancelled.load(Ordering::Relaxed) {
            return Err(FlipperError::TransferCancelled);
        }
        on_progress(idx as u32, total, path);

        // Cheap path first: if we have a cached entry for this file and its
        // mtime hasn't moved, reuse it without re-reading the file body.
        let current_mtime = storage::storage_timestamp(client, path).ok();
        if let (Some(mtime), Some(cached_entry)) = (current_mtime, cached.get(path)) {
            if cached_entry.mtime == Some(mtime) {
                let mut hit = cached_entry.clone();
                hit.mtime = Some(mtime);
                entries.push(hit);
                continue;
            }
        }

        // Cache miss (new file, mtime changed, or timestamp failed): do the
        // full read + parse. `storage_read` failures are non-fatal — a locked
        // or missing file is skipped so one bad entry can't break the scan.
        let bytes = match storage::storage_read(client, path, |_, _| {}, || false) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(?e, %path, "skipping unreadable .sub file");
                continue;
            }
        };

        let text = String::from_utf8_lossy(&bytes);
        let name = library_walk::file_basename(path).to_string();
        let mut entry = parse_sub(path, &name, &text);
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
        // pb_storage::FileType::Dir = 1 in the firmware enum.
        if f.r#type == 1 {
            walk_dir(client, &child, excluded, out)?;
        } else if library_walk::has_extension_ci(&f.name, ".sub")
            && !library_walk::is_excluded(&child, excluded)
        {
            out.push(child);
        }
    }
    Ok(())
}

/// Parse a .sub file's text body into a [`SubGhzEntry`].
/// Public so unit tests in this file (and future tooling) can hit it directly.
pub fn parse_sub(path: &str, name: &str, text: &str) -> SubGhzEntry {
    let mut frequency = None;
    let mut preset = None;
    let mut protocol = None;
    let mut bit = None;
    let mut te = None;
    let mut key = None;
    let mut has_raw = false;
    let mut lat_explicit: Option<f64> = None;
    let mut lon_explicit: Option<f64> = None;
    let mut coord_pair_fallback: Option<(f64, f64)> = None;

    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        match k {
            "Frequency" => frequency = v.parse::<u32>().ok(),
            "Preset" => preset = Some(v.to_string()),
            "Protocol" => protocol = Some(v.to_string()),
            "Bit" => bit = v.parse::<u32>().ok(),
            "TE" => te = v.parse::<u32>().ok(),
            "Key" => key = Some(v.to_string()),
            "RAW_Data" => has_raw = true,
            "Latitude" | "Lat" => {
                lat_explicit = parse_coordinate_value(v, CoordinateAxis::Latitude)
            }
            "Longitude" | "Lon" | "Lng" => {
                lon_explicit = parse_coordinate_value(v, CoordinateAxis::Longitude)
            }
            // Single-line coord fields — Note/Comment/GPS/Coordinates may carry "lat,lon".
            "GPS" | "Coordinates" | "Coords" | "Note" | "Comment" | "Location"
                if coord_pair_fallback.is_none() =>
            {
                coord_pair_fallback = parse_coord_pair(v);
            }
            _ => {}
        }
    }

    let coordinates = match (lat_explicit, lon_explicit) {
        (Some(lat), Some(lon)) if valid_coord(lat, lon) => Some(Coordinates { lat, lon }),
        _ => coord_pair_fallback
            .filter(|(lat, lon)| valid_coord(*lat, *lon))
            .map(|(lat, lon)| Coordinates { lat, lon }),
    };

    let modulation = preset
        .as_deref()
        .map(modulation_from_preset)
        .map(String::from);

    SubGhzEntry {
        path: path.to_string(),
        name: name.to_string(),
        frequency,
        preset,
        protocol,
        bit,
        te,
        key,
        modulation,
        coordinates,
        has_raw,
        mtime: None,
    }
}

/// Map a Flipper preset string to a coarse modulation label.
fn modulation_from_preset(preset: &str) -> &'static str {
    let p = preset.to_ascii_lowercase();
    if p.contains("ook") {
        "OOK"
    } else if p.contains("fm") {
        "FM"
    } else {
        "Unknown"
    }
}

#[derive(Clone, Copy)]
enum CoordinateAxis {
    Latitude,
    Longitude,
}

fn parse_coordinate_value(s: &str, axis: CoordinateAxis) -> Option<f64> {
    let mut tokens = s.split_whitespace();
    let (value, attached_hemisphere) = parse_coordinate_token(tokens.next()?)?;
    let trailing_token = tokens.next();
    let trailing_hemisphere = trailing_token.and_then(parse_hemisphere_token);

    // An attached suffix and a second standalone suffix are ambiguous, as are
    // standalone combinations such as "N S". Reject them instead of silently
    // choosing the last character.
    if attached_hemisphere.is_some() && trailing_hemisphere.is_some() {
        return None;
    }
    if trailing_token.is_some_and(looks_like_hemisphere_fragment) && trailing_hemisphere.is_none() {
        return None;
    }
    if tokens.any(looks_like_hemisphere_fragment) {
        return None;
    }

    apply_hemisphere(value, attached_hemisphere.or(trailing_hemisphere), axis)
}

/// Parse the first two floats out of a string, separated by comma/space/semicolon.
fn parse_coord_pair(s: &str) -> Option<(f64, f64)> {
    let tokens: Vec<&str> = s
        .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .filter(|token| !token.is_empty())
        .collect();
    let mut components = Vec::with_capacity(2);
    let mut index = 0;

    while index < tokens.len() {
        let token = tokens[index];
        let Some((value, attached_hemisphere)) = parse_coordinate_token(token) else {
            // Direction-only fragments such as "NS" indicate a malformed or
            // conflicting coordinate rather than harmless note text.
            if looks_like_hemisphere_fragment(token) {
                return None;
            }
            index += 1;
            continue;
        };

        let standalone_hemisphere = tokens
            .get(index + 1)
            .and_then(|token| parse_hemisphere_token(token).inspect(|_| index += 1));
        if attached_hemisphere.is_some() && standalone_hemisphere.is_some() {
            return None;
        }

        let axis = match components.len() {
            0 => CoordinateAxis::Latitude,
            1 => CoordinateAxis::Longitude,
            // Keep the existing contract: note/comment text may contain other
            // numbers after the first coordinate pair, and those are ignored.
            _ => break,
        };
        components.push(apply_hemisphere(
            value,
            attached_hemisphere.or(standalone_hemisphere),
            axis,
        )?);
        index += 1;
    }

    Some((*components.first()?, *components.get(1)?))
}

fn parse_coordinate_token(token: &str) -> Option<(f64, Option<char>)> {
    let token = token.trim_matches(|c: char| matches!(c, '(' | ')' | '[' | ']'));
    let hemisphere = token
        .chars()
        .last()
        .filter(|character| is_hemisphere(*character));
    let number = if hemisphere.is_some() {
        &token[..token.len() - 1]
    } else {
        token
    }
    .trim_end_matches('\u{b0}');

    Some((number.parse::<f64>().ok()?, hemisphere))
}

fn parse_hemisphere_token(token: &str) -> Option<char> {
    let mut characters = token.chars();
    let hemisphere = characters.next()?;
    (characters.next().is_none() && is_hemisphere(hemisphere)).then_some(hemisphere)
}

fn looks_like_hemisphere_fragment(token: &str) -> bool {
    let mut characters = token.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    is_hemisphere(first) && characters.clone().count() <= 1 && characters.all(is_hemisphere)
}

fn is_hemisphere(character: char) -> bool {
    matches!(character.to_ascii_uppercase(), 'N' | 'S' | 'E' | 'W')
}

fn apply_hemisphere(value: f64, hemisphere: Option<char>, axis: CoordinateAxis) -> Option<f64> {
    let Some(hemisphere) = hemisphere.map(|character| character.to_ascii_uppercase()) else {
        return Some(value);
    };

    let (suffix_axis, sign) = match hemisphere {
        'N' => (CoordinateAxis::Latitude, 1.0),
        'S' => (CoordinateAxis::Latitude, -1.0),
        'E' => (CoordinateAxis::Longitude, 1.0),
        'W' => (CoordinateAxis::Longitude, -1.0),
        _ => return None,
    };
    if !matches!(
        (axis, suffix_axis),
        (CoordinateAxis::Latitude, CoordinateAxis::Latitude)
            | (CoordinateAxis::Longitude, CoordinateAxis::Longitude)
    ) {
        return None;
    }

    // A negative number with N/E conflicts with the suffix. A negative number
    // with S/W is redundant but consistent and remains negative.
    if value.is_sign_negative() && sign > 0.0 {
        return None;
    }
    Some(value.abs() * sign)
}

fn valid_coord(lat: f64, lon: f64) -> bool {
    (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) && !(lat == 0.0 && lon == 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_keyfile() {
        let text = "\
Filetype: Flipper SubGhz Key File
Version: 1
Frequency: 433920000
Preset: FuriHalSubGhzPresetOok650Async
Protocol: Princeton
Bit: 24
Key: 00 00 00 00 00 AB CD EF
TE: 400
";
        let e = parse_sub("/ext/subghz/foo.sub", "foo.sub", text);
        assert_eq!(e.frequency, Some(433_920_000));
        assert_eq!(e.preset.as_deref(), Some("FuriHalSubGhzPresetOok650Async"));
        assert_eq!(e.protocol.as_deref(), Some("Princeton"));
        assert_eq!(e.bit, Some(24));
        assert_eq!(e.te, Some(400));
        assert_eq!(e.modulation.as_deref(), Some("OOK"));
        assert!(!e.has_raw);
        assert!(e.coordinates.is_none());
    }

    #[test]
    fn parses_raw_capture() {
        let text = "Filetype: Flipper SubGhz RAW File\nFrequency: 868350000\nPreset: FuriHalSubGhzPresetFmDev2_38Async\nProtocol: RAW\nRAW_Data: 100 -200 100 -200\n";
        let e = parse_sub("/ext/subghz/r.sub", "r.sub", text);
        assert_eq!(e.protocol.as_deref(), Some("RAW"));
        assert_eq!(e.modulation.as_deref(), Some("FM"));
        assert!(e.has_raw);
        assert!(e.bit.is_none());
    }

    #[test]
    fn extracts_explicit_coords() {
        let text = "Frequency: 433920000\nLatitude: 48.8584\nLongitude: 2.2945\n";
        let e = parse_sub("/p", "n", text);
        let c = e.coordinates.expect("coords parsed");
        assert!((c.lat - 48.8584).abs() < 1e-6);
        assert!((c.lon - 2.2945).abs() < 1e-6);
    }

    #[test]
    fn applies_explicit_north_east_hemispheres() {
        let text = "Latitude: 48.8584 N\nLongitude: 2.2945E\n";
        let e = parse_sub("/p", "n", text);
        let c = e.coordinates.expect("north/east coordinates parsed");
        assert!((c.lat - 48.8584).abs() < 1e-6);
        assert!((c.lon - 2.2945).abs() < 1e-6);
    }

    #[test]
    fn applies_explicit_south_west_hemispheres() {
        let text = "Latitude: 33.8688 S\nLongitude: 151.2093W\n";
        let e = parse_sub("/p", "n", text);
        let c = e.coordinates.expect("south/west coordinates parsed");
        assert!((c.lat + 33.8688).abs() < 1e-6);
        assert!((c.lon + 151.2093).abs() < 1e-6);
    }

    #[test]
    fn extracts_pair_from_note_field() {
        let text = "Frequency: 433920000\nNote: GPS 48.8584, 2.2945 - eiffel\n";
        let e = parse_sub("/p", "n", text);
        let c = e.coordinates.expect("coords parsed from note");
        assert!((c.lat - 48.8584).abs() < 1e-6);
        assert!((c.lon - 2.2945).abs() < 1e-6);
    }

    #[test]
    fn applies_hemispheres_to_coordinate_pairs() {
        let text = "Coordinates: 33.8688S, 151.2093 E\n";
        let e = parse_sub("/p", "n", text);
        let c = e.coordinates.expect("suffixed coordinate pair parsed");
        assert!((c.lat + 33.8688).abs() < 1e-6);
        assert!((c.lon - 151.2093).abs() < 1e-6);
    }

    #[test]
    fn rejects_invalid_coords() {
        let text = "Latitude: 999\nLongitude: 0\n";
        let e = parse_sub("/p", "n", text);
        assert!(e.coordinates.is_none());
    }

    #[test]
    fn rejects_wrong_axis_and_conflicting_hemispheres() {
        for text in [
            "Latitude: 48 E\nLongitude: 2 N\n",
            "Latitude: -48 N\nLongitude: 2 E\n",
            "Latitude: 48 N S\nLongitude: 2 E\n",
            "Latitude: 48 NS\nLongitude: 2 E\n",
            "Coordinates: 48 NS, 2 E\n",
            "Coordinates: 48 E, 2 N\n",
        ] {
            let e = parse_sub("/p", "n", text);
            assert!(
                e.coordinates.is_none(),
                "unexpected coordinates for {text:?}"
            );
        }
    }

    #[test]
    fn excluded_path_logic() {
        let excluded = vec!["/ext/subghz/private".to_string()];
        assert!(library_walk::is_excluded("/ext/subghz/private", &excluded));
        assert!(library_walk::is_excluded(
            "/ext/subghz/private/x.sub",
            &excluded
        ));
        assert!(!library_walk::is_excluded(
            "/ext/subghz/public/x.sub",
            &excluded
        ));
        assert!(!library_walk::is_excluded(
            "/ext/subghz/private2",
            &excluded
        ));
    }
}
