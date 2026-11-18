//! BLE device discovery.
//!
//! Scans for peripherals and keeps the ones that look like a Flipper: any of the
//! advertised `0x3080 | body colour` service UUIDs, the cached Serial service
//! UUID, or a name containing "flipper". Entries are tagged with RSSI and a
//! best-effort `paired` flag derived from cached services.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use btleplug::api::{
    Central, CentralEvent, CentralState, Peripheral as _, PeripheralProperties, ScanFilter,
};
use btleplug::platform::PeripheralId;
use futures::StreamExt;
use tauri::{AppHandle, Emitter};

use crate::error::{FlipperError, Result};
use crate::flipper::ble::{
    runtime::{shared_adapter, BLE_RT},
    ADVERTISED_SERVICES, SERIAL_SERVICE,
};

/// A BLE device the user could connect to.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BleDevice {
    /// Peripheral ID in btleplug's string form — opaque to the frontend but
    /// stable for the lifetime of the adapter/session.
    pub id: String,
    pub name: String,
    pub rssi: Option<i16>,
    /// Heuristic: true iff the peripheral has cached services (implying the OS
    /// has completed bonding and service discovery at least once).
    pub paired: bool,
}

fn map_btle_err(err: impl std::fmt::Display) -> FlipperError {
    FlipperError::Internal(format!("BLE error: {err}"))
}

/// Shown in the BLE dialog while the host's Bluetooth radio is off.
const BLUETOOTH_OFF: &str =
    "Bluetooth is turned off on this computer. Turn it on and the scan will continue.";

/// Which of the Flipper signals an advertisement carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FlipperMatch {
    /// One of the `0x3080 | colour` UUIDs from the primary advertisement.
    advertised: bool,
    /// The custom Serial service UUID. Never advertised — it only shows up
    /// once the OS has bonded and cached a service discovery, so it doubles
    /// as the "paired" hint.
    serial: bool,
    /// Name heuristic. Stock firmware advertises "Flipper <name>", but
    /// Momentum advertises the bare device name, so this can't be relied on.
    name: bool,
}

impl FlipperMatch {
    fn any(self) -> bool {
        self.advertised || self.serial || self.name
    }
}

fn match_flipper(name: &str, services: &[uuid::Uuid]) -> FlipperMatch {
    FlipperMatch {
        advertised: services.iter().any(|s| ADVERTISED_SERVICES.contains(s)),
        serial: services.contains(&SERIAL_SERVICE),
        name: name.to_lowercase().contains("flipper"),
    }
}

/// Turn a scanned peripheral into a dialog entry, or `None` if it isn't a
/// Flipper.
fn flipper_device(id: String, props: &PeripheralProperties) -> Option<BleDevice> {
    let name = props.local_name.clone().unwrap_or_default();
    let m = match_flipper(&name, &props.services);
    if !m.any() {
        return None;
    }
    Some(BleDevice {
        id,
        name: if name.is_empty() {
            "Flipper (unknown)".into()
        } else {
            name
        },
        rssi: props.rssi,
        // `paired` drives the green icon and sort order in the dialog. Cached
        // SERIAL_SERVICE is a strong signal the OS has bonded already; an
        // advertised UUID only means the device is in range.
        paired: m.serial,
    })
}

/// Tell the BLE dialog why the scan can't find anything (`None` clears it).
fn emit_problem(app: &AppHandle, problem: Option<&str>) {
    let _ = app.emit("ble-scan-problem", problem);
}

/// User-facing text for an adapter that couldn't be opened.
fn adapter_problem(err: &FlipperError) -> String {
    match err {
        FlipperError::Internal(msg) => msg.clone(),
        other => other.to_string(),
    }
}

/// Scan for Flipper devices for ~1.8 s and return what's discoverable.
/// Returns an empty list on systems with no BLE adapter.
pub async fn list_ble_devices() -> Result<Vec<BleDevice>> {
    // No adapter on this host (e.g. Bluetooth disabled, VM without passthrough)
    // is not an error — the UI renders an empty list and prompts the user.
    let Ok(adapter) = shared_adapter().await else {
        return Ok(vec![]);
    };

    // Drain pending events, start an unfiltered scan, wait a bit, then enumerate
    // peripherals. btleplug caches properties so the subsequent `properties()`
    // call returns whatever was seen during the scan.
    //
    // We intentionally do NOT pass a service-UUID ScanFilter: Flipper Zero does
    // not include the Serial service UUID in its primary advertisement — it's
    // only visible after connecting and doing service discovery. On macOS
    // CoreBluetooth a UUID-filtered scan would therefore return zero hits.
    let mut events = adapter.events().await.map_err(map_btle_err)?;
    adapter
        .start_scan(ScanFilter::default())
        .await
        .map_err(map_btle_err)?;

    // Pump events for ~10s — this lets btleplug's caches populate and gives
    // slower-advertising devices a chance to show up.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(10_000);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, events.next()).await {
            Ok(Some(CentralEvent::DeviceDiscovered(_))) => {}
            Ok(Some(CentralEvent::DeviceUpdated(_))) => {}
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break, // timeout reached
        }
    }

    let _ = adapter.stop_scan().await;

    let peripherals = adapter.peripherals().await.map_err(map_btle_err)?;
    tracing::info!("BLE scan saw {} peripheral(s)", peripherals.len());
    let mut out = Vec::new();
    for p in peripherals {
        let props = match p.properties().await {
            Ok(Some(pr)) => pr,
            _ => {
                tracing::debug!("BLE skip {:?}: no properties", p.id());
                continue;
            }
        };
        let name = props.local_name.clone().unwrap_or_default();
        let m = match_flipper(&name, &props.services);
        tracing::info!(
            "BLE candidate id={} name={:?} services={} rssi={:?} adv:{} serial:{} name:{}",
            p.id(),
            name,
            props.services.len(),
            props.rssi,
            m.advertised,
            m.serial,
            m.name,
        );
        if let Some(device) = flipper_device(p.id().to_string(), &props) {
            out.push(device);
        }
    }
    tracing::info!("BLE scan returning {} Flipper candidate(s)", out.len());
    Ok(out)
}

/// Blocking wrapper — safe to call from `spawn_blocking` contexts.
pub fn list_ble_devices_blocking() -> Result<Vec<BleDevice>> {
    BLE_RT.block_on(list_ble_devices())
}

/// Inspect a single peripheral and emit a `ble-scan-device` event if it looks
/// like a Flipper. Returns silently for non-matches and error cases — the live
/// scan loop should never bail on a single peripheral hiccup.
async fn maybe_emit_peripheral(
    adapter: &btleplug::platform::Adapter,
    id: &PeripheralId,
    app: &AppHandle,
) {
    let Ok(p) = adapter.peripheral(id).await else {
        return;
    };
    let Ok(Some(props)) = p.properties().await else {
        return;
    };
    let Some(device) = flipper_device(p.id().to_string(), &props) else {
        return;
    };
    let _ = app.emit("ble-scan-device", &device);
}

/// Run a live BLE scan that emits `ble-scan-device` Tauri events as Flipper
/// peripherals are discovered or updated, and stops only when `cancel` flips to
/// false. Emits `ble-scan-stopped` when the loop exits.
///
/// Both `DeviceDiscovered` and `DeviceUpdated` events are forwarded so RSSI
/// values refresh while the dialog is open — the frontend dedupes by `id`.
pub async fn live_scan(app: AppHandle, cancel: Arc<AtomicBool>) -> Result<()> {
    // Always clear the running flag and notify the UI the scan has ended,
    // regardless of which exit path is taken below.
    let result = (async {
        let adapter = match shared_adapter().await {
            Ok(adapter) => adapter,
            Err(e) => {
                // No adapter or no permission. Without this the dialog would
                // spin on "Looking for Flippers…" forever.
                tracing::warn!("BLE live scan: adapter unavailable: {e}");
                emit_problem(&app, Some(&adapter_problem(&e)));
                return Ok::<(), FlipperError>(());
            }
        };

        let mut events = adapter.events().await.map_err(map_btle_err)?;
        if matches!(adapter.adapter_state().await, Ok(CentralState::PoweredOff)) {
            emit_problem(&app, Some(BLUETOOTH_OFF));
        }
        // CoreBluetooth ignores a scan started while the radio is off, so the
        // StateUpdate arm below restarts it once the radio powers on.
        adapter
            .start_scan(ScanFilter::default())
            .await
            .map_err(map_btle_err)?;

        // Replay any peripherals btleplug already cached so users with bonded
        // devices see them instantly instead of waiting for the next adv packet.
        if let Ok(cached) = adapter.peripherals().await {
            for p in cached {
                maybe_emit_peripheral(&adapter, &p.id(), &app).await;
            }
        }

        // Pump events until cancellation. The 250 ms timeout bounds how long
        // we can sit blocked when the device is silent — the cancel flag is
        // checked between every wakeup.
        while cancel.load(Ordering::Relaxed) {
            match tokio::time::timeout(Duration::from_millis(250), events.next()).await {
                Ok(Some(CentralEvent::DeviceDiscovered(id)))
                | Ok(Some(CentralEvent::DeviceUpdated(id))) => {
                    maybe_emit_peripheral(&adapter, &id, &app).await;
                }
                Ok(Some(CentralEvent::StateUpdate(state))) => {
                    tracing::info!("BLE adapter state changed: {state:?}");
                    match state {
                        CentralState::PoweredOn => {
                            emit_problem(&app, None);
                            let _ = adapter.start_scan(ScanFilter::default()).await;
                        }
                        CentralState::PoweredOff => emit_problem(&app, Some(BLUETOOTH_OFF)),
                        CentralState::Unknown => {}
                    }
                }
                Ok(Some(_)) => {} // ignore connect/disconnect/services events
                Ok(None) => break,
                Err(_) => continue, // timeout — recheck cancel
            }
        }

        let _ = adapter.stop_scan().await;
        Ok(())
    })
    .await;

    cancel.store(false, Ordering::Relaxed);
    let _ = app.emit("ble-scan-stopped", ());
    result
}

/// Blocking wrapper for `live_scan`. Safe to call from `spawn_blocking`.
pub fn live_scan_blocking(app: AppHandle, cancel: Arc<AtomicBool>) -> Result<()> {
    BLE_RT.block_on(live_scan(app, cancel))
}

#[cfg(test)]
mod tests {
    use btleplug::api::bleuuid::uuid_from_u16;
    use btleplug::api::PeripheralProperties;

    use super::{flipper_device, match_flipper};
    use crate::flipper::ble::SERIAL_SERVICE;

    fn props(name: Option<&str>, services: Vec<uuid::Uuid>) -> PeripheralProperties {
        PeripheralProperties {
            local_name: name.map(str::to_owned),
            services,
            rssi: Some(-60),
            ..Default::default()
        }
    }

    #[test]
    fn every_body_colour_uuid_matches() {
        // 0x3080 | colour: unknown, black, white, transparent.
        for short in [0x3080, 0x3081, 0x3082, 0x3083] {
            let m = match_flipper("", &[uuid_from_u16(short)]);
            assert!(m.advertised, "0x{short:04x} should match");
        }
    }

    #[test]
    fn momentum_bare_name_is_found_by_uuid() {
        // Momentum advertises the device name without the "Flipper " prefix.
        let device = flipper_device(
            "id".into(),
            &props(Some("Ulefo"), vec![uuid_from_u16(0x3082)]),
        )
        .expect("white Momentum Flipper should be listed");
        assert_eq!(device.name, "Ulefo");
        assert!(!device.paired);
    }

    #[test]
    fn unrelated_devices_are_skipped() {
        assert!(flipper_device(
            "id".into(),
            &props(Some("Ulefo"), vec![uuid_from_u16(0x180f)])
        )
        .is_none());
        assert!(flipper_device("id".into(), &props(None, vec![uuid_from_u16(0x3084)])).is_none());
        assert!(flipper_device("id".into(), &props(Some("AirPods"), vec![])).is_none());
    }

    #[test]
    fn stock_name_and_cached_serial_still_match() {
        assert!(match_flipper("Flipper Ulefo", &[]).name);
        let device = flipper_device("id".into(), &props(None, vec![SERIAL_SERVICE]))
            .expect("bonded Flipper should be listed");
        assert_eq!(device.name, "Flipper (unknown)");
        assert!(device.paired);
    }
}
