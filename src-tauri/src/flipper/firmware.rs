//! Firmware-update plumbing: the modular firmware-source registry, fetching and
//! parsing each source's `directory.json`, downloading + verifying an update
//! bundle, and unpacking the `.tgz` into the flat set of files the Flipper's
//! own updater expects under `/ext/update/<bundle>/`.
//!
//! Everything here is pure (no Tauri, no device IO) so it stays unit-testable
//! and reusable. The orchestration that actually talks to the device lives in
//! `commands::firmware`.
//!
//! ## Adding a custom firmware
//!
//! Every mainline Flipper firmware (official, Unleashed, Momentum, RogueMaster,
//! …) publishes the *same* `directory.json` schema produced by the upstream
//! update-server tooling. Supporting a new one is therefore a single entry in
//! [`PROVIDERS`] — give it an id, a display name, and the `directory.json` URL.
//! No per-provider parsing code is required.

use std::io::Read;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{FlipperError, Result};

/// Flipper Zero hardware target. The f7 (STM32WB55) is the only Flipper Zero
/// hardware, so every update bundle we want is the `f7` / `update_tgz` file.
const HW_TARGET: &str = "f7";
const UPDATE_FILE_TYPE: &str = "update_tgz";

/// Channels we surface, in display priority order. Forks sometimes publish
/// extra per-PR channels (e.g. Momentum's `pr503:…`); restricting to this set
/// keeps the picker clean across every provider.
const KNOWN_CHANNELS: &[&str] = &["release", "release-candidate", "development"];

/// A selectable firmware source. Add a new fork by appending one entry — the
/// `directory.json` schema is shared across all mainline Flipper firmwares.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirmwareProvider {
    /// Stable id used by the frontend and `firmware_fetch_directory`.
    pub id: &'static str,
    /// Human-facing name for the source dropdown.
    pub name: &'static str,
    /// Short tagline shown under the name.
    pub blurb: &'static str,
    /// `directory.json` endpoint.
    pub directory_url: &'static str,
}

/// The registry. To add a custom firmware, append a row here.
pub const PROVIDERS: &[FirmwareProvider] = &[
    FirmwareProvider {
        id: "official",
        name: "Official",
        blurb: "Flipper Devices stock firmware",
        directory_url: "https://update.flipperzero.one/firmware/directory.json",
    },
    FirmwareProvider {
        id: "unleashed",
        name: "Unleashed",
        blurb: "Community firmware, fewer regional limits",
        directory_url: "https://up.unleashedflip.com/directory.json",
    },
    FirmwareProvider {
        id: "momentum",
        name: "Momentum",
        blurb: "Feature-rich community firmware",
        directory_url: "https://up.momentum-fw.dev/firmware/directory.json",
    },
];

pub fn provider(id: &str) -> Option<&'static FirmwareProvider> {
    PROVIDERS.iter().find(|p| p.id == id)
}

// ── directory.json wire format ──────────────────────────────────────────────

/// Deserialize a field that may be absent **or** explicitly `null` into its
/// default. Plain `#[serde(default)]` only covers absent fields; the community
/// update servers sometimes emit `"files": null` / `"changelog": null`, which
/// would otherwise fail with "invalid type: null, expected …".
fn null_default<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Deserialize)]
struct RawDirectory {
    #[serde(default, deserialize_with = "null_default")]
    channels: Vec<RawChannel>,
}

#[derive(Debug, Deserialize)]
struct RawChannel {
    #[serde(default, deserialize_with = "null_default")]
    id: String,
    #[serde(default, deserialize_with = "null_default")]
    title: String,
    #[serde(default, deserialize_with = "null_default")]
    description: String,
    #[serde(default, deserialize_with = "null_default")]
    versions: Vec<RawVersion>,
}

#[derive(Debug, Deserialize)]
struct RawVersion {
    #[serde(default, deserialize_with = "null_default")]
    version: String,
    #[serde(default, deserialize_with = "null_default")]
    changelog: String,
    #[serde(default, deserialize_with = "null_default")]
    timestamp: u64,
    #[serde(default, deserialize_with = "null_default")]
    files: Vec<RawFile>,
}

#[derive(Debug, Deserialize)]
struct RawFile {
    #[serde(default, deserialize_with = "null_default")]
    url: String,
    #[serde(default, deserialize_with = "null_default")]
    target: String,
    #[serde(rename = "type", default, deserialize_with = "null_default")]
    file_type: String,
    #[serde(default, deserialize_with = "null_default")]
    sha256: String,
}

// ── UI-facing, normalized catalog ───────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirmwareCatalog {
    pub provider_id: String,
    pub channels: Vec<ChannelInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub id: String,
    pub title: String,
    pub description: String,
    pub versions: Vec<VersionInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: String,
    pub changelog: String,
    /// Unix epoch seconds of the build, 0 when the source omits it.
    pub timestamp: u64,
    /// Direct URL of the `f7` `update_tgz` bundle.
    pub url: String,
    /// Expected SHA-256 (hex), empty when the source omits it.
    pub sha256: String,
}

fn channel_rank(id: &str) -> usize {
    KNOWN_CHANNELS
        .iter()
        .position(|c| *c == id)
        .unwrap_or(usize::MAX)
}

/// Fetch and normalize a provider's `directory.json` into the f7 update bundles
/// grouped by channel. Drops channels/versions with no usable f7 `update_tgz`.
pub fn fetch_catalog(provider_id: &str, directory_url: &str) -> Result<FirmwareCatalog> {
    let body = http_get_string(directory_url)?;
    let raw: RawDirectory = serde_json::from_str(&body)
        .map_err(|e| FlipperError::Internal(format!("directory.json parse error: {e}")))?;

    let mut channels: Vec<ChannelInfo> = raw
        .channels
        .into_iter()
        .filter(|c| KNOWN_CHANNELS.contains(&c.id.as_str()))
        .map(|c| {
            let versions = c
                .versions
                .into_iter()
                .filter_map(|v| {
                    let file = v.files.into_iter().find(|f| {
                        f.file_type == UPDATE_FILE_TYPE
                            && f.target == HW_TARGET
                            && !f.url.is_empty()
                    })?;
                    Some(VersionInfo {
                        version: v.version,
                        changelog: v.changelog,
                        timestamp: v.timestamp,
                        url: file.url,
                        sha256: file.sha256,
                    })
                })
                .collect::<Vec<_>>();
            ChannelInfo {
                id: c.id,
                title: c.title,
                description: c.description,
                versions,
            }
        })
        .filter(|c| !c.versions.is_empty())
        .collect();

    channels.sort_by_key(|c| channel_rank(&c.id));

    Ok(FirmwareCatalog {
        provider_id: provider_id.to_string(),
        channels,
    })
}

// ── download + verify ───────────────────────────────────────────────────────

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .user_agent(concat!("FlipperUI/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn http_get_string(url: &str) -> Result<String> {
    agent()
        .get(url)
        .call()
        .map_err(|e| FlipperError::Internal(format!("HTTP request failed: {e}")))?
        .into_string()
        .map_err(|e| FlipperError::Internal(format!("HTTP read failed: {e}")))
}

/// Stream a URL into memory, reporting `(downloaded, total)` after each chunk.
/// `total` is 0 when the server omits Content-Length. Aborts with
/// `TransferCancelled` when `cancelled()` flips true between chunks.
pub fn download<F>(url: &str, on_progress: F, cancelled: &dyn Fn() -> bool) -> Result<Vec<u8>>
where
    F: Fn(u64, u64),
{
    let resp = agent()
        .get(url)
        .call()
        .map_err(|e| FlipperError::Internal(format!("download request failed: {e}")))?;

    let total: u64 = resp
        .header("Content-Length")
        .and_then(|h| h.parse().ok())
        .unwrap_or(0);

    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 64 * 1024];
    let mut out: Vec<u8> = Vec::with_capacity(total as usize);
    on_progress(0, total);
    loop {
        if cancelled() {
            return Err(FlipperError::TransferCancelled);
        }
        let n = reader
            .read(&mut buf)
            .map_err(|e| FlipperError::Internal(format!("download read error: {e}")))?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        on_progress(out.len() as u64, total);
    }
    Ok(out)
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ── unpack ──────────────────────────────────────────────────────────────────

/// One file inside an update bundle, with its path relative to the bundle's
/// top-level directory (e.g. `firmware.dfu`, or `sub/dir/file` if nested).
#[derive(Debug, Clone)]
pub struct BundleFile {
    pub rel_path: String,
    pub data: Vec<u8>,
}

/// A fully-unpacked update bundle ready to push to the device.
#[derive(Debug, Clone)]
pub struct UpdateBundle {
    /// Top-level directory name inside the archive, e.g. `f7-update-1.4.3`.
    pub top_dir: String,
    /// The manifest's relative path within the bundle (always `update.fuf`).
    pub manifest_rel: String,
    pub files: Vec<BundleFile>,
}

impl UpdateBundle {
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.data.len() as u64).sum()
    }
}

const MANIFEST_NAME: &str = "update.fuf";

/// Unpack an `f7` update bundle into its constituent files.
///
/// Accepts both a gzip-compressed `.tgz` (the standard distribution form, incl.
/// custom firmware) and an already-decompressed `.tar` — the gzip magic bytes
/// (`1f 8b`) pick the path, so a user can drop either. Update bundles are a
/// single top-level directory of flat files (`firmware.dfu`, `radio.bin`,
/// `resources.ths`, `update.fuf`, …); we keep paths relative to that directory
/// and locate the `update.fuf` manifest so the caller can build the on-device
/// manifest path.
pub fn unpack_update_archive(bytes: &[u8]) -> Result<UpdateBundle> {
    let is_gzip = bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b;
    let reader: Box<dyn Read> = if is_gzip {
        Box::new(flate2::read::GzDecoder::new(bytes))
    } else {
        Box::new(bytes) // &[u8] implements Read — treat as a plain tar
    };
    let mut archive = tar::Archive::new(reader);

    let mut top_dir: Option<String> = None;
    let mut files: Vec<BundleFile> = Vec::new();
    let mut manifest_rel: Option<String> = None;

    let entries = archive
        .entries()
        .map_err(|e| FlipperError::Internal(format!("update bundle is not a valid tar: {e}")))?;

    for entry in entries {
        let mut entry =
            entry.map_err(|e| FlipperError::Internal(format!("corrupt update bundle: {e}")))?;
        let etype = entry.header().entry_type();
        if !etype.is_file() {
            continue; // skip directory entries; we recreate dirs on the device
        }
        let path = entry
            .path()
            .map_err(|e| FlipperError::Internal(format!("bad path in update bundle: {e}")))?
            .to_string_lossy()
            .replace('\\', "/");

        let mut comps = path.splitn(2, '/');
        let first = comps.next().unwrap_or_default().to_string();
        let rest = comps.next().map(|s| s.to_string());

        // The archive must be rooted at a single top-level directory.
        let rel = match rest {
            Some(r) if !r.is_empty() => {
                match &top_dir {
                    Some(d) if *d != first => {
                        return Err(FlipperError::Internal(
                            "update bundle has multiple top-level directories".into(),
                        ));
                    }
                    None => top_dir = Some(first.clone()),
                    _ => {}
                }
                r
            }
            // A file at the archive root (no wrapping dir) — tolerate it.
            _ => first,
        };

        let mut data = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut data)
            .map_err(|e| FlipperError::Internal(format!("read error in update bundle: {e}")))?;

        if rel == MANIFEST_NAME || rel.ends_with(&format!("/{MANIFEST_NAME}")) {
            manifest_rel = Some(rel.clone());
        }
        files.push(BundleFile {
            rel_path: rel,
            data,
        });
    }

    if files.is_empty() {
        return Err(FlipperError::Internal("update bundle is empty".into()));
    }
    let manifest_rel = manifest_rel.ok_or_else(|| {
        FlipperError::Internal(format!("update bundle is missing {MANIFEST_NAME}"))
    })?;

    Ok(UpdateBundle {
        top_dir: top_dir.unwrap_or_else(|| "update".to_string()),
        manifest_rel,
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vector() {
        // SHA-256 of the empty string.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn channel_rank_orders_release_first() {
        assert!(channel_rank("release") < channel_rank("development"));
        assert!(channel_rank("release-candidate") < channel_rank("development"));
        assert_eq!(channel_rank("pr503:foo"), usize::MAX);
    }

    #[test]
    fn unpack_flat_bundle_finds_manifest_and_top_dir() {
        // Build a tiny gzip'd tar mirroring a real bundle's flat layout.
        let mut tar_buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            for (name, body) in [
                ("f7-update-9.9.9/update.fuf", b"manifest".as_slice()),
                ("f7-update-9.9.9/firmware.dfu", b"dfu".as_slice()),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_entry_type(tar::EntryType::file());
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, name, body).unwrap();
            }
            builder.finish().unwrap();
        }
        let mut gz = Vec::new();
        {
            use std::io::Write;
            let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
            enc.write_all(&tar_buf).unwrap();
            enc.finish().unwrap();
        }

        let bundle = unpack_update_archive(&gz).unwrap();
        assert_eq!(bundle.top_dir, "f7-update-9.9.9");
        assert_eq!(bundle.manifest_rel, "update.fuf");
        assert_eq!(bundle.files.len(), 2);
        assert_eq!(bundle.total_bytes(), 11);

        // The same archive uncompressed (plain .tar) must unpack identically.
        let plain = unpack_update_archive(&tar_buf).unwrap();
        assert_eq!(plain.top_dir, "f7-update-9.9.9");
        assert_eq!(plain.files.len(), 2);
    }
}
