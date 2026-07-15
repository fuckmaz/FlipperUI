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
//! [`PROVIDERS`] — give it an id, display metadata, the `directory.json` URL,
//! and exact HTTPS host allowlists for catalogs and downloads. No per-provider
//! parsing code is required.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::error::{FlipperError, Result};

/// Flipper Zero hardware target. The f7 (STM32WB55) is the only Flipper Zero
/// hardware, so every update bundle we want is the `f7` / `update_tgz` file.
const HW_TARGET: &str = "f7";
const UPDATE_FILE_TYPE: &str = "update_tgz";

/// Hard resource limits for untrusted firmware inputs. Current update bundles
/// are comfortably below these ceilings; the limits primarily prevent a
/// malformed local archive or compromised update server from exhausting the
/// desktop process before the Flipper ever sees the data.
pub const MAX_FIRMWARE_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_CATALOG_BYTES: u64 = 4 * 1024 * 1024;
const MAX_EXPANDED_BYTES: u64 = 512 * 1024 * 1024;
const MAX_DECOMPRESSED_TAR_BYTES: u64 = MAX_EXPANDED_BYTES + 64 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 8_192;
const MAX_ARCHIVE_FILES: usize = 4_096;
const MAX_ARCHIVE_PATH_BYTES: usize = 512;
const MAX_ARCHIVE_COMPONENT_BYTES: usize = 128;
const MAX_ARCHIVE_PATH_DEPTH: usize = 16;
const MAX_HTTP_REDIRECTS: usize = 3;
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_IO_TIMEOUT: Duration = Duration::from_secs(10);
const CATALOG_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_HTTP_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Channels we surface, in display priority order. Forks sometimes publish
/// extra per-PR channels (e.g. Momentum's `pr503:…`); restricting to this set
/// keeps the picker clean across every provider.
const KNOWN_CHANNELS: &[&str] = &["release", "release-candidate", "development"];

/// A selectable firmware source. Add a new fork by appending one entry — the
/// `directory.json` schema is shared across all mainline Flipper firmwares.
#[derive(Debug, Clone)]
pub struct FirmwareProvider {
    /// Stable id used by the frontend and `firmware_fetch_directory`.
    pub id: &'static str,
    /// Human-facing name for the source dropdown.
    pub name: &'static str,
    /// Short tagline shown under the name.
    pub blurb: &'static str,
    /// `directory.json` endpoint.
    pub directory_url: &'static str,
    /// Exact HTTPS hosts accepted while fetching the catalog (including each
    /// hop in a redirect chain).
    pub catalog_hosts: &'static [&'static str],
    /// Exact HTTPS hosts accepted for update bundles and their redirects.
    pub download_hosts: &'static [&'static str],
}

/// The registry. To add a custom firmware, append a row here.
pub const PROVIDERS: &[FirmwareProvider] = &[
    FirmwareProvider {
        id: "official",
        name: "Official",
        blurb: "Flipper Devices stock firmware",
        directory_url: "https://update.flipperzero.one/firmware/directory.json",
        catalog_hosts: &["update.flipperzero.one"],
        download_hosts: &["update.flipperzero.one"],
    },
    FirmwareProvider {
        id: "unleashed",
        name: "Unleashed",
        blurb: "Community firmware, fewer regional limits",
        directory_url: "https://up.unleashedflip.com/directory.json",
        catalog_hosts: &["up.unleashedflip.com"],
        download_hosts: &["unleashedflip.com"],
    },
    FirmwareProvider {
        id: "momentum",
        name: "Momentum",
        blurb: "Feature-rich community firmware",
        directory_url: "https://up.momentum-fw.dev/firmware/directory.json",
        catalog_hosts: &["up.momentum-fw.dev"],
        download_hosts: &["up.momentum-fw.dev"],
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
    /// Opaque backend-generated binding to the security-relevant catalog
    /// fields. The frontend echoes this when starting a flash.
    pub selection_token: String,
}

/// Backend-resolved online bundle. The webview never supplies these fields;
/// they are obtained from a freshly fetched, registered provider catalog.
#[derive(Debug, Clone)]
pub struct ResolvedFirmware {
    pub url: String,
    pub sha256: String,
    pub label: String,
}

fn channel_rank(id: &str) -> usize {
    KNOWN_CHANNELS
        .iter()
        .position(|c| *c == id)
        .unwrap_or(usize::MAX)
}

fn fetch_raw_catalog(provider: &FirmwareProvider) -> Result<RawDirectory> {
    let body = http_get_string(
        provider.directory_url,
        provider.catalog_hosts,
        MAX_CATALOG_BYTES,
        CATALOG_HTTP_TIMEOUT,
    )?;
    let raw: RawDirectory = serde_json::from_str(&body)
        .map_err(|e| FlipperError::Internal(format!("directory.json parse error: {e}")))?;
    Ok(raw)
}

fn trusted_update_file<'a>(
    provider: &FirmwareProvider,
    files: &'a [RawFile],
) -> Result<&'a RawFile> {
    let mut candidates = files
        .iter()
        .filter(|f| f.file_type == UPDATE_FILE_TYPE && f.target == HW_TARGET);
    let file = candidates
        .next()
        .ok_or_else(|| FlipperError::Internal("catalog entry has no f7 update bundle".into()))?;
    if candidates.next().is_some() {
        return Err(FlipperError::Internal(
            "catalog entry has multiple f7 update bundles".into(),
        ));
    }
    validate_sha256(&file.sha256)?;
    validate_trusted_https_url(&file.url, provider.download_hosts)?;
    Ok(file)
}

fn selection_fingerprint(
    provider_id: &str,
    channel_id: &str,
    version: &str,
    timestamp: u64,
    target: &str,
    file_type: &str,
    sha256: &str,
) -> String {
    let mut digest = Sha256::new();
    for field in [provider_id, channel_id, version, target, file_type, sha256] {
        digest.update((field.len() as u64).to_be_bytes());
        digest.update(field.as_bytes());
    }
    digest.update(timestamp.to_be_bytes());
    let bytes = digest.finalize();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn normalize_catalog(provider: &FirmwareProvider, raw: RawDirectory) -> FirmwareCatalog {
    let mut channels: Vec<ChannelInfo> = raw
        .channels
        .into_iter()
        .filter(|c| KNOWN_CHANNELS.contains(&c.id.as_str()))
        .map(|c| {
            let channel_id = c.id.clone();
            let versions = c
                .versions
                .into_iter()
                // Only expose entries the backend can later authenticate and
                // download. A bad catalog entry is unavailable, not optional-
                // checksum firmware.
                .filter_map(|v| {
                    let file = trusted_update_file(provider, &v.files).ok()?;
                    Some(VersionInfo {
                        selection_token: selection_fingerprint(
                            provider.id,
                            &channel_id,
                            &v.version,
                            v.timestamp,
                            &file.target,
                            &file.file_type,
                            &file.sha256.to_ascii_lowercase(),
                        ),
                        version: v.version,
                        changelog: v.changelog,
                        timestamp: v.timestamp,
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

    FirmwareCatalog {
        provider_id: provider.id.to_string(),
        channels,
    }
}

/// Fetch and normalize a registered provider's `directory.json` into the f7
/// update bundles grouped by channel. Entries without a mandatory valid
/// SHA-256 or with an untrusted URL are not exposed to the webview.
pub fn fetch_catalog(provider: &FirmwareProvider) -> Result<FirmwareCatalog> {
    Ok(normalize_catalog(provider, fetch_raw_catalog(provider)?))
}

/// Refetch a registered provider's catalog and resolve exactly the selected
/// channel/version/build. This is the remote-flash trust boundary: no URL or
/// checksum supplied by the webview participates in the result.
pub fn resolve_firmware(
    provider: &FirmwareProvider,
    channel_id: &str,
    version: &str,
    timestamp: u64,
    selection_token: &str,
) -> Result<ResolvedFirmware> {
    resolve_firmware_from_catalog(
        provider,
        fetch_raw_catalog(provider)?,
        channel_id,
        version,
        timestamp,
        selection_token,
    )
}

/// Pure half of remote selection resolution, kept separate from the catalog
/// fetch so exact-selection failures can be tested without network access.
fn resolve_firmware_from_catalog(
    provider: &FirmwareProvider,
    raw: RawDirectory,
    channel_id: &str,
    version: &str,
    timestamp: u64,
    selection_token: &str,
) -> Result<ResolvedFirmware> {
    if channel_id.len() > 128 || version.len() > 128 {
        return Err(FlipperError::Internal(
            "firmware selection identifier is too long".into(),
        ));
    }
    let channel = raw
        .channels
        .into_iter()
        .find(|c| c.id == channel_id && KNOWN_CHANNELS.contains(&c.id.as_str()))
        .ok_or_else(|| FlipperError::Internal("selected firmware channel is unavailable".into()))?;

    let mut versions = channel
        .versions
        .iter()
        .filter(|v| v.version == version && v.timestamp == timestamp);
    let selected = versions.next().ok_or_else(|| {
        FlipperError::Internal(
            "selected firmware build is no longer present in the provider catalog".into(),
        )
    })?;
    if versions.next().is_some() {
        return Err(FlipperError::Internal(
            "selected firmware build is ambiguous in the provider catalog".into(),
        ));
    }

    let file = trusted_update_file(provider, &selected.files)?;
    let expected_token = selection_fingerprint(
        provider.id,
        &channel.id,
        &selected.version,
        selected.timestamp,
        &file.target,
        &file.file_type,
        &file.sha256.to_ascii_lowercase(),
    );
    if selection_token.len() != 64 || selection_token != expected_token {
        return Err(FlipperError::Internal(
            "firmware catalog selection changed; reload the catalog before flashing".into(),
        ));
    }
    let channel_name = if channel.title.is_empty() {
        channel.id
    } else {
        channel.title
    };
    Ok(ResolvedFirmware {
        url: file.url.clone(),
        sha256: file.sha256.to_ascii_lowercase(),
        label: format!("{} · {channel_name} {}", provider.name, selected.version),
    })
}

// ── download + verify ───────────────────────────────────────────────────────

fn agent(connect_timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(connect_timeout)
        .timeout_read(HTTP_IO_TIMEOUT)
        .timeout_write(HTTP_IO_TIMEOUT)
        // Redirects are followed manually so every destination is checked
        // against the provider's exact HTTPS host allowlist.
        .redirects(0)
        .user_agent(concat!("FlipperUI/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn remaining_http_time(
    started: Instant,
    overall_timeout: Duration,
    what: &str,
) -> Result<Duration> {
    overall_timeout
        .checked_sub(started.elapsed())
        .ok_or_else(|| FlipperError::Internal(format!("{what} exceeded its overall timeout")))
}

/// Apply the remaining end-to-end budget to the ureq request itself. In ureq
/// 2.12, `Request::timeout` creates an absolute deadline that covers response
/// headers and is retained by the `DeadlineStream` returned from
/// `Response::into_reader`, so a blocked body read also cannot outlive this
/// budget. DNS resolution remains the library/OS-level exception: ureq cannot
/// interrupt a resolver call once it has entered the platform resolver.
fn call_with_remaining_deadline(
    url: &str,
    started: Instant,
    overall_timeout: Duration,
    what: &str,
) -> Result<ureq::Response> {
    let remaining = remaining_http_time(started, overall_timeout, what)?;
    // ureq documents connect_timeout as taking precedence over the request
    // deadline, so clamp it to the same remaining budget for this hop.
    let connect_timeout = HTTP_CONNECT_TIMEOUT.min(remaining);
    agent(connect_timeout)
        .get(url)
        .timeout(remaining)
        .call()
        .map_err(|e| FlipperError::Internal(format!("{what} failed: {e}")))
}

fn validate_trusted_https_url(raw: &str, allowed_hosts: &[&str]) -> Result<Url> {
    let parsed = Url::parse(raw)
        .map_err(|e| FlipperError::Internal(format!("invalid firmware URL: {e}")))?;
    if parsed.scheme() != "https" {
        return Err(FlipperError::Internal("firmware URL must use HTTPS".into()));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(FlipperError::Internal(
            "firmware URL must not contain credentials".into(),
        ));
    }
    if parsed.port().is_some_and(|port| port != 443) {
        return Err(FlipperError::Internal(
            "firmware URL must use the standard HTTPS port".into(),
        ));
    }
    if parsed.fragment().is_some() {
        return Err(FlipperError::Internal(
            "firmware URL must not contain a fragment".into(),
        ));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| FlipperError::Internal("firmware URL has no host".into()))?;
    if !allowed_hosts
        .iter()
        .any(|allowed| host.eq_ignore_ascii_case(allowed))
    {
        return Err(FlipperError::Internal(format!(
            "firmware URL host is not trusted: {host}"
        )));
    }
    Ok(parsed)
}

fn redirect_target(current: &Url, location: &str, allowed_hosts: &[&str]) -> Result<Url> {
    let joined = current
        .join(location)
        .map_err(|e| FlipperError::Internal(format!("invalid firmware redirect: {e}")))?;
    validate_trusted_https_url(joined.as_str(), allowed_hosts)
}

fn trusted_get(
    url: &str,
    allowed_hosts: &[&str],
    overall_timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Result<(ureq::Response, Instant)> {
    let mut current = validate_trusted_https_url(url, allowed_hosts)?;
    let started = Instant::now();
    for redirects in 0..=MAX_HTTP_REDIRECTS {
        if cancelled() {
            return Err(FlipperError::TransferCancelled);
        }
        let response = call_with_remaining_deadline(
            current.as_str(),
            started,
            overall_timeout,
            "firmware HTTP request",
        );
        if cancelled() {
            return Err(FlipperError::TransferCancelled);
        }
        let response = response?;
        if matches!(response.status(), 301 | 302 | 303 | 307 | 308) {
            if redirects == MAX_HTTP_REDIRECTS {
                return Err(FlipperError::Internal(
                    "firmware request exceeded the redirect limit".into(),
                ));
            }
            let location = response.header("Location").ok_or_else(|| {
                FlipperError::Internal("firmware redirect has no Location header".into())
            })?;
            current = redirect_target(&current, location, allowed_hosts)?;
            continue;
        }
        if !(200..300).contains(&response.status()) {
            return Err(FlipperError::Internal(format!(
                "firmware server returned HTTP {}",
                response.status()
            )));
        }
        return Ok((response, started));
    }
    Err(FlipperError::Internal(
        "firmware request exceeded the redirect limit".into(),
    ))
}

fn read_response_limited(
    response: ureq::Response,
    max_bytes: u64,
    what: &str,
    started: Instant,
    overall_timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<u8>> {
    let declared = response
        .header("Content-Length")
        .and_then(|h| h.parse::<u64>().ok());
    if declared.is_some_and(|len| len > max_bytes) {
        return Err(FlipperError::Internal(format!(
            "{what} exceeds the {} byte limit",
            max_bytes
        )));
    }
    let mut reader = response.into_reader();
    let mut bytes =
        Vec::with_capacity(declared.unwrap_or(0).min(max_bytes).try_into().unwrap_or(0));
    let mut chunk = [0u8; 64 * 1024];
    loop {
        if cancelled() {
            return Err(FlipperError::TransferCancelled);
        }
        remaining_http_time(started, overall_timeout, what)?;
        // The reader inherits the same absolute ureq request deadline. A
        // cancellation that arrives during this blocking call is observed as
        // soon as the read returns; it cannot interrupt the syscall itself.
        let read = match reader.read(&mut chunk) {
            Ok(read) => read,
            Err(_) if cancelled() => return Err(FlipperError::TransferCancelled),
            Err(error) => {
                return Err(FlipperError::Internal(format!(
                    "{what} read failed: {error}"
                )))
            }
        };
        if read == 0 {
            break;
        }
        if (bytes.len() as u64).saturating_add(read as u64) > max_bytes {
            return Err(FlipperError::Internal(format!(
                "{what} exceeds the {} byte limit",
                max_bytes
            )));
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Ok(bytes)
}

fn http_get_string(
    url: &str,
    allowed_hosts: &[&str],
    max_bytes: u64,
    overall_timeout: Duration,
) -> Result<String> {
    let never_cancelled = || false;
    let (response, started) = trusted_get(url, allowed_hosts, overall_timeout, &never_cancelled)?;
    let bytes = read_response_limited(
        response,
        max_bytes,
        "catalog",
        started,
        overall_timeout,
        &never_cancelled,
    )?;
    String::from_utf8(bytes)
        .map_err(|e| FlipperError::Internal(format!("catalog is not valid UTF-8: {e}")))
}

/// Stream a URL into memory, reporting `(downloaded, total)` after each chunk.
/// `total` is 0 when the server omits Content-Length. Aborts with
/// `TransferCancelled` when `cancelled()` flips true between chunks.
pub fn download<F>(
    url: &str,
    allowed_hosts: &[&str],
    on_progress: F,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<u8>>
where
    F: Fn(u64, u64),
{
    let (resp, started) = trusted_get(url, allowed_hosts, DOWNLOAD_HTTP_TIMEOUT, cancelled)?;

    read_download_response(resp, started, DOWNLOAD_HTTP_TIMEOUT, on_progress, cancelled)
}

fn read_download_response<F>(
    resp: ureq::Response,
    started: Instant,
    overall_timeout: Duration,
    on_progress: F,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<u8>>
where
    F: Fn(u64, u64),
{
    let total: u64 = resp
        .header("Content-Length")
        .and_then(|h| h.parse().ok())
        .unwrap_or(0);
    if total > MAX_FIRMWARE_ARCHIVE_BYTES {
        return Err(FlipperError::Internal(format!(
            "firmware download exceeds the {} byte limit",
            MAX_FIRMWARE_ARCHIVE_BYTES
        )));
    }

    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 64 * 1024];
    let mut out: Vec<u8> = Vec::with_capacity(
        total
            .min(MAX_FIRMWARE_ARCHIVE_BYTES)
            .try_into()
            .unwrap_or(0),
    );
    on_progress(0, total);
    loop {
        if cancelled() {
            return Err(FlipperError::TransferCancelled);
        }
        remaining_http_time(started, overall_timeout, "firmware download")?;
        let n = match reader.read(&mut buf) {
            Ok(read) => read,
            Err(_) if cancelled() => return Err(FlipperError::TransferCancelled),
            Err(error) => {
                return Err(FlipperError::Internal(format!(
                    "download read error: {error}"
                )))
            }
        };
        if n == 0 {
            break;
        }
        if (out.len() as u64).saturating_add(n as u64) > MAX_FIRMWARE_ARCHIVE_BYTES {
            return Err(FlipperError::Internal(format!(
                "firmware download exceeds the {} byte limit",
                MAX_FIRMWARE_ARCHIVE_BYTES
            )));
        }
        out.extend_from_slice(&buf[..n]);
        on_progress(out.len() as u64, total);
    }
    Ok(out)
}

/// Read a local firmware archive without trusting its extension or metadata.
/// The post-read limit also covers a file that grows after it is opened.
pub fn read_local_archive(path: &Path) -> Result<Vec<u8>> {
    let file = File::open(path)?;
    let declared = file.metadata()?.len();
    if declared > MAX_FIRMWARE_ARCHIVE_BYTES {
        return Err(FlipperError::Internal(format!(
            "local firmware archive exceeds the {} byte limit",
            MAX_FIRMWARE_ARCHIVE_BYTES
        )));
    }
    let mut reader = file.take(MAX_FIRMWARE_ARCHIVE_BYTES + 1);
    let mut bytes = Vec::with_capacity(declared.try_into().unwrap_or(0));
    reader.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FIRMWARE_ARCHIVE_BYTES {
        return Err(FlipperError::Internal(format!(
            "local firmware archive exceeds the {} byte limit",
            MAX_FIRMWARE_ARCHIVE_BYTES
        )));
    }
    Ok(bytes)
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(FlipperError::Internal(
            "catalog entry has no valid SHA-256 checksum".into(),
        ));
    }
    Ok(())
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
const MAX_UPDATE_MANIFEST_BYTES: u64 = 64 * 1024;
const UPDATE_MANIFEST_FILETYPE: &str = "Flipper firmware upgrade configuration";
const UPDATE_MANIFEST_VERSION: &str = "2";

#[derive(Clone, Copy)]
struct ArchiveLimits {
    archive_bytes: u64,
    decompressed_bytes: u64,
    expanded_bytes: u64,
    file_bytes: u64,
    entries: usize,
    files: usize,
    path_bytes: usize,
    component_bytes: usize,
    path_depth: usize,
}

const ARCHIVE_LIMITS: ArchiveLimits = ArchiveLimits {
    archive_bytes: MAX_FIRMWARE_ARCHIVE_BYTES,
    decompressed_bytes: MAX_DECOMPRESSED_TAR_BYTES,
    expanded_bytes: MAX_EXPANDED_BYTES,
    file_bytes: MAX_FILE_BYTES,
    entries: MAX_ARCHIVE_ENTRIES,
    files: MAX_ARCHIVE_FILES,
    path_bytes: MAX_ARCHIVE_PATH_BYTES,
    component_bytes: MAX_ARCHIVE_COMPONENT_BYTES,
    path_depth: MAX_ARCHIVE_PATH_DEPTH,
};

/// Enforces a limit on all decompressed tar bytes, including headers, padding,
/// and PAX/GNU metadata that the `tar` crate consumes before yielding an entry.
/// File-size accounting alone cannot protect those internal metadata reads.
struct SizeLimitedReader<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> SizeLimitedReader<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
        }
    }
}

impl<R: Read> Read for SizeLimitedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "decompressed update bundle exceeds its size limit",
                )),
            };
        }
        let allowed = usize::try_from(self.remaining)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let read = self.inner.read(&mut buf[..allowed])?;
        self.remaining = self.remaining.saturating_sub(read as u64);
        Ok(read)
    }
}

fn archive_error(message: impl Into<String>) -> FlipperError {
    FlipperError::Internal(format!("unsafe update bundle: {}", message.into()))
}

/// Validate an archive path independently of the host OS. Backslashes are
/// rejected rather than normalized so a bundle cannot mean one thing during
/// inspection and another when its path is sent to the Flipper.
fn validate_archive_path(raw: &[u8], limits: ArchiveLimits) -> Result<Vec<&str>> {
    if raw.is_empty() || raw.len() > limits.path_bytes {
        return Err(archive_error("entry path is empty or too long"));
    }
    let has_windows_drive_prefix = raw.len() >= 2 && raw[0].is_ascii_alphabetic() && raw[1] == b':';
    if raw.starts_with(b"/") || raw.contains(&b'\\') || has_windows_drive_prefix {
        return Err(archive_error(
            "absolute paths and backslashes are not allowed",
        ));
    }

    // Tar directory entries commonly carry one trailing slash. Strip exactly
    // one before component validation; repeated slashes remain invalid.
    let raw = raw.strip_suffix(b"/").unwrap_or(raw);
    if raw.is_empty() {
        return Err(archive_error("entry path is empty"));
    }
    let path =
        std::str::from_utf8(raw).map_err(|_| archive_error("entry path is not valid UTF-8"))?;
    let components: Vec<&str> = path.split('/').collect();
    if components.len() > limits.path_depth {
        return Err(archive_error("entry path is too deep"));
    }
    for component in &components {
        if component.is_empty() || *component == "." || *component == ".." {
            return Err(archive_error(
                "empty, current, and parent path components are not allowed",
            ));
        }
        if component.len() > limits.component_bytes {
            return Err(archive_error("entry path component is too long"));
        }
        if component.chars().any(|character| character.is_control()) {
            return Err(archive_error("entry path contains control characters"));
        }
        if component.ends_with(['.', ' ']) {
            return Err(archive_error(
                "entry path component must not end with a dot or space",
            ));
        }
        if component
            .chars()
            .any(|character| matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        {
            return Err(archive_error(
                "entry path contains characters invalid on FAT/exFAT",
            ));
        }
        let fat_stem = component
            .split_once('.')
            .map_or(*component, |(stem, _)| stem)
            .to_ascii_uppercase();
        let reserved_numbered = fat_stem
            .strip_prefix("COM")
            .or_else(|| fat_stem.strip_prefix("LPT"))
            .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'));
        if matches!(fat_stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
            || reserved_numbered
        {
            return Err(archive_error("entry path uses a reserved device name"));
        }
    }
    Ok(components)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArchiveNodeKind {
    Directory,
    File,
}

fn normalized_device_path(components: &[&str]) -> String {
    // Unicode lowercasing is a conservative approximation of FAT/exFAT's
    // case-insensitive namespace. Rejecting extra collisions is safer than
    // allowing two archive entries to overwrite the same device path.
    components.join("/").to_lowercase()
}

fn register_archive_namespace(
    components: &[&str],
    entry_kind: ArchiveNodeKind,
    explicit_entries: &mut HashSet<String>,
    namespace: &mut HashMap<String, ArchiveNodeKind>,
) -> Result<()> {
    let explicit_path = normalized_device_path(components);
    if !explicit_entries.insert(explicit_path.clone()) {
        return Err(archive_error(format!(
            "duplicate archive entry: {}",
            components.join("/")
        )));
    }

    for depth in 1..=components.len() {
        let expected = if depth == components.len() {
            entry_kind
        } else {
            ArchiveNodeKind::Directory
        };
        let prefix = normalized_device_path(&components[..depth]);
        match namespace.get(&prefix) {
            Some(existing) if *existing != expected => {
                return Err(archive_error(format!(
                    "file/directory namespace conflict: {}",
                    components[..depth].join("/")
                )));
            }
            Some(_) => {}
            None => {
                namespace.insert(prefix, expected);
            }
        }
    }
    Ok(())
}

fn validate_update_manifest(bundle: &UpdateBundle) -> Result<()> {
    if bundle.manifest_rel != MANIFEST_NAME {
        return Err(archive_error(
            "update.fuf must be at the bundle's top level",
        ));
    }
    let manifest = bundle
        .files
        .iter()
        .find(|file| file.rel_path == bundle.manifest_rel)
        .ok_or_else(|| archive_error("manifest entry is missing"))?;
    if manifest.data.len() as u64 > MAX_UPDATE_MANIFEST_BYTES {
        return Err(archive_error("update.fuf is too large"));
    }
    let text = std::str::from_utf8(&manifest.data)
        .map_err(|_| archive_error("update.fuf is not valid UTF-8"))?;
    let mut fields: HashMap<String, String> = HashMap::new();
    for (line_index, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once(':').ok_or_else(|| {
            archive_error(format!(
                "update.fuf line {} is not a key/value field",
                line_index + 1
            ))
        })?;
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() || value.is_empty() {
            return Err(archive_error(format!(
                "update.fuf line {} has an empty key or value",
                line_index + 1
            )));
        }
        let normalized_key = key.to_ascii_lowercase();
        if fields
            .insert(normalized_key.clone(), value.to_string())
            .is_some()
        {
            return Err(archive_error(format!(
                "update.fuf contains duplicate key: {key}"
            )));
        }
    }

    let filetype = fields
        .get("filetype")
        .ok_or_else(|| archive_error("update.fuf is missing Filetype"))?;
    if filetype != UPDATE_MANIFEST_FILETYPE {
        return Err(archive_error(format!(
            "update.fuf has unsupported Filetype: {filetype}"
        )));
    }

    let version = fields
        .get("version")
        .ok_or_else(|| archive_error("update.fuf is missing Version"))?;
    if version != UPDATE_MANIFEST_VERSION {
        return Err(archive_error(format!(
            "update.fuf has unsupported Version: {version}"
        )));
    }

    let target = fields
        .get("target")
        .ok_or_else(|| archive_error("update.fuf is missing Target"))?;
    if target != "7" {
        return Err(archive_error(format!(
            "update.fuf targets {target}, expected 7"
        )));
    }

    for required in ["loader", "firmware"] {
        if !fields.contains_key(required) {
            return Err(archive_error(format!(
                "update.fuf is missing required {required} reference"
            )));
        }
    }

    let available_files: HashSet<String> = bundle
        .files
        .iter()
        .map(|file| file.rel_path.to_lowercase())
        .collect();
    let mut referenced_files = HashSet::new();
    for key in ["loader", "firmware", "radio", "resources", "splashscreen"] {
        let Some(reference) = fields.get(key) else {
            continue;
        };
        let components = validate_archive_path(reference.as_bytes(), ARCHIVE_LIMITS)
            .map_err(|_| archive_error(format!("update.fuf {key} path is unsafe")))?;
        let normalized = normalized_device_path(&components);
        if !referenced_files.insert(normalized.clone()) {
            return Err(archive_error(format!(
                "update.fuf references the same file more than once: {reference}"
            )));
        }
        if !available_files.contains(&normalized) {
            return Err(archive_error(format!(
                "update.fuf references missing file: {reference}"
            )));
        }
    }
    Ok(())
}

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
    unpack_update_archive_with_limits(bytes, ARCHIVE_LIMITS)
}

fn unpack_update_archive_with_limits(bytes: &[u8], limits: ArchiveLimits) -> Result<UpdateBundle> {
    if bytes.len() as u64 > limits.archive_bytes {
        return Err(archive_error("compressed archive is too large"));
    }
    let is_gzip = bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b;
    let reader: Box<dyn Read> = if is_gzip {
        Box::new(flate2::read::GzDecoder::new(bytes))
    } else {
        Box::new(bytes) // &[u8] implements Read — treat as a plain tar
    };
    let mut archive = tar::Archive::new(SizeLimitedReader::new(reader, limits.decompressed_bytes));

    let mut top_dir: Option<String> = None;
    let mut files: Vec<BundleFile> = Vec::new();
    let mut manifest_rel: Option<String> = None;
    let mut total_expanded = 0u64;
    let mut entry_count = 0usize;
    let mut explicit_entries = HashSet::new();
    let mut namespace = HashMap::new();

    let entries = archive
        .entries()
        .map_err(|e| FlipperError::Internal(format!("update bundle is not a valid tar: {e}")))?;

    for entry in entries {
        let mut entry =
            entry.map_err(|e| FlipperError::Internal(format!("corrupt update bundle: {e}")))?;
        entry_count = entry_count
            .checked_add(1)
            .ok_or_else(|| archive_error("too many entries"))?;
        if entry_count > limits.entries {
            return Err(archive_error("too many entries"));
        }

        let etype = entry.header().entry_type();
        if !etype.is_file() && !etype.is_dir() {
            return Err(archive_error(
                "links and special archive entries are not allowed",
            ));
        }
        let path_bytes = entry.path_bytes().into_owned();
        if etype.is_file() && path_bytes.ends_with(b"/") {
            return Err(archive_error("file path must not end with a slash"));
        }
        let components = validate_archive_path(&path_bytes, limits)?;
        let first = components[0];
        match &top_dir {
            Some(dir) if dir != first => {
                return Err(archive_error("multiple top-level directories"));
            }
            None => top_dir = Some(first.to_string()),
            _ => {}
        }

        let node_kind = if etype.is_dir() {
            ArchiveNodeKind::Directory
        } else {
            ArchiveNodeKind::File
        };
        register_archive_namespace(
            &components,
            node_kind,
            &mut explicit_entries,
            &mut namespace,
        )?;

        if etype.is_dir() {
            continue; // safe directory; the uploader recreates it as needed
        }
        if components.len() < 2 {
            return Err(archive_error(
                "files must be contained in one top-level directory",
            ));
        }
        if files.len() >= limits.files {
            return Err(archive_error("too many files"));
        }
        let is_manifest = components.last() == Some(&MANIFEST_NAME);
        let rel = components[1..].join("/");

        let size = entry.size();
        if is_manifest && size > MAX_UPDATE_MANIFEST_BYTES {
            return Err(archive_error("update.fuf is too large"));
        }
        if size > limits.file_bytes {
            return Err(archive_error(format!("file is too large: {rel}")));
        }
        total_expanded = total_expanded
            .checked_add(size)
            .ok_or_else(|| archive_error("expanded size overflow"))?;
        if total_expanded > limits.expanded_bytes {
            return Err(archive_error("expanded archive is too large"));
        }

        let mut data = Vec::with_capacity(size.try_into().unwrap_or(0));
        entry
            .read_to_end(&mut data)
            .map_err(|e| FlipperError::Internal(format!("read error in update bundle: {e}")))?;
        if data.len() as u64 != size {
            return Err(archive_error(format!(
                "file size does not match its header: {rel}"
            )));
        }

        if is_manifest {
            if manifest_rel.is_some() {
                return Err(archive_error(format!(
                    "bundle contains multiple {MANIFEST_NAME} manifests"
                )));
            }
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

    let bundle = UpdateBundle {
        top_dir: top_dir.ok_or_else(|| archive_error("bundle has no top-level directory"))?,
        manifest_rel,
        files,
    };
    validate_update_manifest(&bundle)?;
    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;

    use super::*;

    const VALID_MANIFEST: &[u8] = b"Filetype: Flipper firmware upgrade configuration\nVersion: 2\nTarget: 7\nLoader: updater.bin\nFirmware: firmware.dfu\n";

    fn append_raw_entry(
        builder: &mut tar::Builder<&mut Vec<u8>>,
        path: &[u8],
        body: &[u8],
        entry_type: tar::EntryType,
    ) {
        assert!(path.len() <= 100, "test helper uses the old tar name field");
        let mut header = tar::Header::new_old();
        header.as_old_mut().name[..path.len()].copy_from_slice(path);
        header.set_size(body.len() as u64);
        header.set_entry_type(entry_type);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, body).unwrap();
    }

    fn raw_tar(entries: &[(&[u8], &[u8], tar::EntryType)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            for (path, body, entry_type) in entries {
                append_raw_entry(&mut builder, path, body, *entry_type);
            }
            builder.finish().unwrap();
        }
        bytes
    }

    fn valid_tar(extra: &[(&[u8], &[u8], tar::EntryType)]) -> Vec<u8> {
        let mut entries = vec![
            (
                b"f7-update/update.fuf".as_slice(),
                VALID_MANIFEST,
                tar::EntryType::file(),
            ),
            (
                b"f7-update/updater.bin".as_slice(),
                b"updater".as_slice(),
                tar::EntryType::file(),
            ),
            (
                b"f7-update/firmware.dfu".as_slice(),
                b"firmware".as_slice(),
                tar::EntryType::file(),
            ),
        ];
        entries.extend_from_slice(extra);
        raw_tar(&entries)
    }

    fn test_limits() -> ArchiveLimits {
        ArchiveLimits {
            archive_bytes: 32 * 1024,
            decompressed_bytes: 32 * 1024,
            expanded_bytes: 512,
            file_bytes: 256,
            entries: 8,
            files: 6,
            path_bytes: 80,
            component_bytes: 40,
            path_depth: 6,
        }
    }

    fn resolvable_catalog() -> RawDirectory {
        RawDirectory {
            channels: vec![RawChannel {
                id: "release".into(),
                title: "Release".into(),
                description: String::new(),
                versions: vec![RawVersion {
                    version: "1.4.3".into(),
                    changelog: String::new(),
                    timestamp: 1_765_000_000,
                    files: vec![RawFile {
                        url: "https://update.flipperzero.one/firmware-1.4.3.tgz".into(),
                        target: HW_TARGET.into(),
                        file_type: UPDATE_FILE_TYPE.into(),
                        sha256: "a".repeat(64),
                    }],
                }],
            }],
        }
    }

    fn resolvable_selection_token() -> String {
        selection_fingerprint(
            "official",
            "release",
            "1.4.3",
            1_765_000_000,
            HW_TARGET,
            UPDATE_FILE_TYPE,
            &"a".repeat(64),
        )
    }

    fn spawn_stalled_http_server(response_prefix: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            if !response_prefix.is_empty() {
                stream.write_all(response_prefix).unwrap();
                stream.flush().unwrap();
            }
            thread::sleep(Duration::from_millis(500));
        });
        format!("http://{address}/firmware")
    }

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
    fn unknown_provider_is_not_registered() {
        assert!(provider("not-a-provider").is_none());
    }

    #[test]
    fn pure_resolver_rejects_unavailable_catalog_selection() {
        let official = provider("official").unwrap();
        let missing_channel = resolve_firmware_from_catalog(
            official,
            resolvable_catalog(),
            "development",
            "1.4.3",
            1_765_000_000,
            &resolvable_selection_token(),
        )
        .unwrap_err();
        assert!(missing_channel
            .to_string()
            .contains("channel is unavailable"));

        let missing_version = resolve_firmware_from_catalog(
            official,
            resolvable_catalog(),
            "release",
            "9.9.9",
            1_765_000_000,
            &resolvable_selection_token(),
        )
        .unwrap_err();
        assert!(missing_version
            .to_string()
            .contains("build is no longer present"));
    }

    #[test]
    fn pure_resolver_rejects_timestamp_mismatch() {
        let error = resolve_firmware_from_catalog(
            provider("official").unwrap(),
            resolvable_catalog(),
            "release",
            "1.4.3",
            1_765_000_001,
            &resolvable_selection_token(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("build is no longer present"));
    }

    #[test]
    fn pure_resolver_binds_selection_to_catalog_fingerprint() {
        let official = provider("official").unwrap();
        assert!(resolve_firmware_from_catalog(
            official,
            resolvable_catalog(),
            "release",
            "1.4.3",
            1_765_000_000,
            &resolvable_selection_token(),
        )
        .is_ok());
        let error = resolve_firmware_from_catalog(
            official,
            resolvable_catalog(),
            "release",
            "1.4.3",
            1_765_000_000,
            &"0".repeat(64),
        )
        .unwrap_err();
        assert!(error.to_string().contains("catalog selection changed"));
    }

    #[test]
    fn trusted_urls_require_exact_https_hosts_and_safe_redirects() {
        let hosts = &["updates.example.com"];
        assert!(validate_trusted_https_url("https://updates.example.com/file.tgz", hosts).is_ok());
        for bad in [
            "http://updates.example.com/file.tgz",
            "https://updates.example.com.evil.test/file.tgz",
            "https://evil.test/file.tgz",
            "https://user@updates.example.com/file.tgz",
            "https://updates.example.com:8443/file.tgz",
            "https://updates.example.com/file.tgz#fragment",
        ] {
            assert!(
                validate_trusted_https_url(bad, hosts).is_err(),
                "accepted unsafe URL: {bad}"
            );
        }

        let current = Url::parse("https://updates.example.com/releases/current.json").unwrap();
        assert_eq!(
            redirect_target(&current, "../bundle.tgz", hosts)
                .unwrap()
                .as_str(),
            "https://updates.example.com/bundle.tgz"
        );
        assert!(redirect_target(&current, "https://evil.test/bundle.tgz", hosts).is_err());
    }

    #[test]
    fn remaining_deadline_bounds_a_stalled_header_read() {
        let url = spawn_stalled_http_server(b"");
        let started = Instant::now();
        let error =
            call_with_remaining_deadline(&url, started, Duration::from_millis(100), "test request")
                .unwrap_err();
        assert!(error.to_string().contains("test request failed"));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "header deadline was not enforced"
        );
    }

    #[test]
    fn body_deadline_bounds_stalled_read_and_then_observes_cancel() {
        let url = spawn_stalled_http_server(
            b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nx",
        );
        let overall_timeout = Duration::from_millis(150);
        let started = Instant::now();
        let response =
            call_with_remaining_deadline(&url, started, overall_timeout, "test request").unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_writer = Arc::clone(&cancelled);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(25));
            cancel_writer.store(true, Ordering::SeqCst);
        });

        let error = read_download_response(response, started, overall_timeout, |_, _| {}, &|| {
            cancelled.load(Ordering::SeqCst)
        })
        .unwrap_err();
        assert!(matches!(error, FlipperError::TransferCancelled));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "body deadline was not enforced"
        );
    }

    #[test]
    fn catalog_hides_entries_without_trusted_url_and_checksum() {
        let raw = RawDirectory {
            channels: vec![RawChannel {
                id: "release".into(),
                title: "Release".into(),
                description: String::new(),
                versions: vec![
                    RawVersion {
                        version: "good".into(),
                        changelog: String::new(),
                        timestamp: 1,
                        files: vec![RawFile {
                            url: "https://update.flipperzero.one/good.tgz".into(),
                            target: HW_TARGET.into(),
                            file_type: UPDATE_FILE_TYPE.into(),
                            sha256: "a".repeat(64),
                        }],
                    },
                    RawVersion {
                        version: "bad-checksum".into(),
                        changelog: String::new(),
                        timestamp: 2,
                        files: vec![RawFile {
                            url: "https://update.flipperzero.one/bad.tgz".into(),
                            target: HW_TARGET.into(),
                            file_type: UPDATE_FILE_TYPE.into(),
                            sha256: "nope".into(),
                        }],
                    },
                    RawVersion {
                        version: "bad-host".into(),
                        changelog: String::new(),
                        timestamp: 3,
                        files: vec![RawFile {
                            url: "https://update.flipperzero.one.evil.test/bad.tgz".into(),
                            target: HW_TARGET.into(),
                            file_type: UPDATE_FILE_TYPE.into(),
                            sha256: "b".repeat(64),
                        }],
                    },
                ],
            }],
        };
        let catalog = normalize_catalog(provider("official").unwrap(), raw);
        assert_eq!(catalog.channels.len(), 1);
        assert_eq!(catalog.channels[0].versions.len(), 1);
        assert_eq!(catalog.channels[0].versions[0].version, "good");
    }

    #[test]
    fn unpack_flat_bundle_finds_manifest_and_top_dir() {
        // Build a tiny gzip'd tar mirroring a real bundle's flat layout.
        let mut tar_buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            for (name, body) in [
                ("f7-update-9.9.9/update.fuf", VALID_MANIFEST),
                ("f7-update-9.9.9/updater.bin", b"updater".as_slice()),
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
        assert_eq!(bundle.files.len(), 3);
        assert_eq!(bundle.total_bytes(), 123);

        // The same archive uncompressed (plain .tar) must unpack identically.
        let plain = unpack_update_archive(&tar_buf).unwrap();
        assert_eq!(plain.top_dir, "f7-update-9.9.9");
        assert_eq!(plain.files.len(), 3);
    }

    #[test]
    fn unpack_rejects_absolute_parent_backslash_and_empty_components() {
        for bad_path in [
            b"/f7-update/update.fuf".as_slice(),
            b"C:/f7-update/update.fuf".as_slice(),
            b"f7-update/../update.fuf".as_slice(),
            b"f7-update\\update.fuf".as_slice(),
            b"f7-update//update.fuf".as_slice(),
            b"f7-update/./update.fuf".as_slice(),
        ] {
            let tar = raw_tar(&[(bad_path, b"manifest", tar::EntryType::file())]);
            assert!(
                unpack_update_archive(&tar).is_err(),
                "accepted unsafe path: {}",
                String::from_utf8_lossy(bad_path)
            );
        }
    }

    #[test]
    fn unpack_requires_one_root_and_rejects_duplicate_or_root_files() {
        let multiple_roots = raw_tar(&[
            (b"one/update.fuf", b"manifest", tar::EntryType::file()),
            (b"two/file.bin", b"data", tar::EntryType::file()),
        ]);
        assert!(unpack_update_archive(&multiple_roots).is_err());

        let duplicate = raw_tar(&[
            (b"bundle/update.fuf", b"manifest", tar::EntryType::file()),
            (b"bundle/UPDATE.FUF", b"manifest", tar::EntryType::file()),
        ]);
        assert!(unpack_update_archive(&duplicate).is_err());

        let root_file = raw_tar(&[(b"update.fuf", b"manifest", tar::EntryType::file())]);
        assert!(unpack_update_archive(&root_file).is_err());

        let trailing_slash =
            raw_tar(&[(b"bundle/update.fuf/", b"manifest", tar::EntryType::file())]);
        assert!(unpack_update_archive(&trailing_slash).is_err());
    }

    #[test]
    fn unpack_rejects_duplicate_directories_and_namespace_conflicts() {
        let duplicate_dirs = valid_tar(&[
            (b"f7-update/Data/", b"", tar::EntryType::dir()),
            (b"f7-update/data/", b"", tar::EntryType::dir()),
        ]);
        assert!(unpack_update_archive(&duplicate_dirs).is_err());

        let unicode_case_dirs = valid_tar(&[
            ("f7-update/Ä/".as_bytes(), b"", tar::EntryType::dir()),
            ("f7-update/ä/".as_bytes(), b"", tar::EntryType::dir()),
        ]);
        assert!(unpack_update_archive(&unicode_case_dirs).is_err());

        let file_directory_conflict = valid_tar(&[
            (b"f7-update/data/", b"", tar::EntryType::dir()),
            (b"f7-update/data", b"file", tar::EntryType::file()),
        ]);
        assert!(unpack_update_archive(&file_directory_conflict).is_err());

        let file_prefix_conflict = valid_tar(&[
            (b"f7-update/data", b"file", tar::EntryType::file()),
            (
                b"f7-update/data/child.bin",
                b"child",
                tar::EntryType::file(),
            ),
        ]);
        assert!(unpack_update_archive(&file_prefix_conflict).is_err());
    }

    #[test]
    fn unpack_rejects_fat_invalid_and_reserved_component_names() {
        for bad_path in [
            b"f7-update/trailing.".as_slice(),
            b"f7-update/trailing ".as_slice(),
            b"f7-update/bad<name".as_slice(),
            b"f7-update/bad>name".as_slice(),
            b"f7-update/bad:name".as_slice(),
            b"f7-update/bad\"name".as_slice(),
            b"f7-update/bad|name".as_slice(),
            b"f7-update/bad?name".as_slice(),
            b"f7-update/bad*name".as_slice(),
            b"f7-update/CON.txt".as_slice(),
            b"f7-update/aux".as_slice(),
            b"f7-update/COM1.bin".as_slice(),
            b"f7-update/lpt9".as_slice(),
        ] {
            let archive = valid_tar(&[(bad_path, b"data", tar::EntryType::file())]);
            assert!(
                unpack_update_archive(&archive).is_err(),
                "accepted FAT-invalid path: {}",
                String::from_utf8_lossy(bad_path)
            );
        }
    }

    #[test]
    fn update_manifest_requires_full_bundle_identity_and_core_references() {
        let valid = raw_tar(&[
            (b"bundle/update.fuf", VALID_MANIFEST, tar::EntryType::file()),
            (b"bundle/updater.bin", b"updater", tar::EntryType::file()),
            (b"bundle/firmware.dfu", b"firmware", tar::EntryType::file()),
        ]);
        assert!(unpack_update_archive(&valid).is_ok());

        for manifest in [
            b"Version: 2\nTarget: 7\nLoader: updater.bin\nFirmware: firmware.dfu\n".as_slice(),
            b"Filetype: wrong\nVersion: 2\nTarget: 7\nLoader: updater.bin\nFirmware: firmware.dfu\n".as_slice(),
            b"Filetype: Flipper firmware upgrade configuration\nTarget: 7\nLoader: updater.bin\nFirmware: firmware.dfu\n".as_slice(),
            b"Filetype: Flipper firmware upgrade configuration\nVersion: 1\nTarget: 7\nLoader: updater.bin\nFirmware: firmware.dfu\n".as_slice(),
            b"Filetype: Flipper firmware upgrade configuration\nVersion: 2\nTarget: 6\nLoader: updater.bin\nFirmware: firmware.dfu\n".as_slice(),
            b"Filetype: Flipper firmware upgrade configuration\nVersion: 2\nTarget: f7\nLoader: updater.bin\nFirmware: firmware.dfu\n".as_slice(),
            b"Filetype: Flipper firmware upgrade configuration\nVersion: 2\nTarget: 7\nTarget: f7\nLoader: updater.bin\nFirmware: firmware.dfu\n".as_slice(),
            b"Filetype: Flipper firmware upgrade configuration\nVersion: 2\nTarget: 7\nFirmware: firmware.dfu\n".as_slice(),
            b"Filetype: Flipper firmware upgrade configuration\nVersion: 2\nTarget: 7\nLoader: updater.bin\n".as_slice(),
            b"Filetype: Flipper firmware upgrade configuration\nVersion: 2\nTarget: 7\nLoader: missing.bin\nFirmware: firmware.dfu\n".as_slice(),
            b"Filetype: Flipper firmware upgrade configuration\nVersion: 2\nTarget: 7\nLoader: ../updater.bin\nFirmware: firmware.dfu\n".as_slice(),
            b"Filetype: Flipper firmware upgrade configuration\nVersion: 2\nTarget: 7\nLoader: /updater.bin\nFirmware: firmware.dfu\n".as_slice(),
            b"Filetype: Flipper firmware upgrade configuration\nVersion: 2\nTarget: 7\nLoader: updater.bin\nFirmware: updater.bin\n".as_slice(),
            b"Filetype: Flipper firmware upgrade configuration\nVersion: 2\nTarget: 7\nLoader: updater.bin\nFirmware: firmware.dfu\nRadio: missing.bin\n".as_slice(),
        ] {
            let archive = raw_tar(&[
                (b"bundle/update.fuf", manifest, tar::EntryType::file()),
                (b"bundle/updater.bin", b"updater", tar::EntryType::file()),
                (b"bundle/firmware.dfu", b"firmware", tar::EntryType::file()),
            ]);
            assert!(
                unpack_update_archive(&archive).is_err(),
                "accepted invalid manifest: {}",
                String::from_utf8_lossy(manifest)
            );
        }
    }

    #[test]
    fn known_live_manifest_payload_shapes_are_supported() {
        // Official 1.4.3 and Unleashed 089 use resources.ths; Momentum 012
        // currently uses resources.tar.gz. All three declare the same core,
        // radio, resources, and splashscreen fields.
        for resources in ["resources.ths", "resources.tar.gz"] {
            let manifest = format!(
                "Filetype: {UPDATE_MANIFEST_FILETYPE}\nVersion: {UPDATE_MANIFEST_VERSION}\nInfo: current\nTarget: 7\nLoader: updater.bin\nFirmware: firmware.dfu\nRadio: radio.bin\nResources: {resources}\nSplashscreen: splash.bin\n"
            );
            let archive = raw_tar(&[
                (
                    b"bundle/update.fuf",
                    manifest.as_bytes(),
                    tar::EntryType::file(),
                ),
                (b"bundle/updater.bin", b"updater", tar::EntryType::file()),
                (b"bundle/firmware.dfu", b"firmware", tar::EntryType::file()),
                (b"bundle/radio.bin", b"radio", tar::EntryType::file()),
                (
                    format!("bundle/{resources}").as_bytes(),
                    b"resources",
                    tar::EntryType::file(),
                ),
                (b"bundle/splash.bin", b"splash", tar::EntryType::file()),
            ]);
            assert!(unpack_update_archive(&archive).is_ok());
        }
    }

    #[test]
    fn update_manifest_must_be_small_and_at_bundle_root() {
        let oversized = vec![b'x'; MAX_UPDATE_MANIFEST_BYTES as usize + 1];
        let archive = raw_tar(&[(
            b"bundle/update.fuf",
            oversized.as_slice(),
            tar::EntryType::file(),
        )]);
        assert!(unpack_update_archive(&archive).is_err());

        let nested = raw_tar(&[(
            b"bundle/nested/update.fuf",
            VALID_MANIFEST,
            tar::EntryType::file(),
        )]);
        assert!(unpack_update_archive(&nested).is_err());
    }

    #[test]
    fn unpack_rejects_links_control_characters_and_non_utf8_paths() {
        let symlink = valid_tar(&[(b"f7-update/link", b"", tar::EntryType::symlink())]);
        assert!(unpack_update_archive(&symlink).is_err());

        for bad_path in [
            b"f7-update/bad\nname".as_slice(),
            b"f7-update/\xff".as_slice(),
        ] {
            let tar = raw_tar(&[(bad_path, b"manifest", tar::EntryType::file())]);
            assert!(unpack_update_archive(&tar).is_err());
        }
    }

    #[test]
    fn unpack_enforces_archive_resource_limits() {
        let archive = valid_tar(&[(
            b"f7-update/firmware.bin",
            b"firmware",
            tar::EntryType::file(),
        )]);

        let mut limits = test_limits();
        limits.archive_bytes = archive.len() as u64 - 1;
        assert!(unpack_update_archive_with_limits(&archive, limits).is_err());

        let mut limits = test_limits();
        limits.decompressed_bytes = 511;
        assert!(unpack_update_archive_with_limits(&archive, limits).is_err());

        let mut limits = test_limits();
        limits.file_bytes = VALID_MANIFEST.len() as u64 - 1;
        assert!(unpack_update_archive_with_limits(&archive, limits).is_err());

        let mut limits = test_limits();
        limits.expanded_bytes = 135; // one byte below the four files' total
        assert!(unpack_update_archive_with_limits(&archive, limits).is_err());

        let mut limits = test_limits();
        limits.files = 3;
        assert!(unpack_update_archive_with_limits(&archive, limits).is_err());

        let mut limits = test_limits();
        limits.entries = 3;
        assert!(unpack_update_archive_with_limits(&archive, limits).is_err());

        let mut limits = test_limits();
        limits.path_depth = 2;
        let deep = valid_tar(&[(b"f7-update/sub/file.bin", b"data", tar::EntryType::file())]);
        assert!(unpack_update_archive_with_limits(&deep, limits).is_err());

        let mut limits = test_limits();
        limits.path_bytes = 20;
        assert!(unpack_update_archive_with_limits(&archive, limits).is_err());

        let mut limits = test_limits();
        limits.component_bytes = 6;
        assert!(unpack_update_archive_with_limits(&archive, limits).is_err());
    }
}
