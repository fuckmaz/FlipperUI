//! Process-wide Tokio runtime for btleplug.
//!
//! btleplug is async; the rest of FlipperUI runs on blocking threads dispatched
//! from `tauri::async_runtime::spawn_blocking`. Keeping a dedicated multi-thread
//! runtime here lets blocking callers `BLE_RT.block_on(async { ... })` without
//! risk of deadlocking on Tauri's own async runtime.

use btleplug::api::Manager as _;
use btleplug::platform::{Adapter, Manager};
use once_cell::sync::Lazy;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::OnceCell;

use crate::error::{FlipperError, Result};

pub static BLE_RT: Lazy<Runtime> = Lazy::new(|| {
    Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("ble-rt")
        .build()
        .expect("failed to build BLE tokio runtime")
});

/// Process-wide BLE adapter, created on first use. Reused across scans and
/// connects so the adapter's peripheral cache — populated when
/// `list_ble_devices` runs — is still there when the user picks a device and
/// we resolve the id in `connect_ble`. Creating a fresh Manager per call
/// produces a fresh (empty) cache and makes the connect path rely entirely on
/// catching another advertisement within its scan window.
static ADAPTER: OnceCell<Adapter> = OnceCell::const_new();

/// btleplug refuses to open the adapter on macOS when Bluetooth access was
/// denied in Privacy & Security; say so plainly instead of "Permission denied".
fn map_init_err(e: btleplug::Error) -> FlipperError {
    match e {
        btleplug::Error::PermissionDenied if cfg!(target_os = "macos") => FlipperError::Internal(
            "FlipperUI isn't allowed to use Bluetooth. Allow it in System Settings → \
             Privacy & Security → Bluetooth, then restart FlipperUI."
                .into(),
        ),
        btleplug::Error::PermissionDenied => FlipperError::Internal(
            "FlipperUI isn't allowed to use Bluetooth. Check your system's Bluetooth \
             permissions, then restart FlipperUI."
                .into(),
        ),
        e => FlipperError::Internal(format!("BLE error: {e}")),
    }
}

pub async fn shared_adapter() -> Result<Adapter> {
    ADAPTER
        .get_or_try_init(|| async {
            let manager = Manager::new().await.map_err(map_init_err)?;
            let adapters = manager.adapters().await.map_err(map_init_err)?;
            adapters.into_iter().next().ok_or_else(|| {
                FlipperError::Internal("No Bluetooth adapter found on this computer.".into())
            })
        })
        .await
        .cloned()
}
