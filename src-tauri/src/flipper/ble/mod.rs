//! Bluetooth LE transport for the Flipper Zero.
//!
//! Flipper exposes a GATT "Serial" service that carries the same varint-framed
//! protobuf RPC as USB. Unlike USB serial, this service is RPC-only — there is
//! no text CLI mode over BLE. Pairing must be performed at the OS level first;
//! btleplug cannot initiate bonding.

use uuid::{uuid, Uuid};

pub mod connection;
pub mod runtime;
pub mod scanner;
pub mod transport;

/// Flipper BLE Serial service (used post-connect for RPC data).
pub const SERIAL_SERVICE: Uuid = uuid!("8fe5b3d5-2e7f-4a98-2a48-7acc60fe0000");

/// 16-bit service UUIDs Flipper Zero puts in its primary advertisement, as
/// 128-bit UUIDs (Bluetooth base UUID) so they compare directly against
/// `PeripheralProperties.services`. The firmware advertises
/// `0x3080 | body colour` (`serial_profile.c`): 0x3080 unknown, 0x3081 black,
/// 0x3082 white, 0x3083 transparent. Momentum's colour spoof picks from the
/// same set. Scan filtering keys off these — `SERIAL_SERVICE` is not
/// advertised, only discoverable after `connect`.
pub const ADVERTISED_SERVICES: [Uuid; 4] = [
    btleplug::api::bleuuid::uuid_from_u16(0x3080),
    btleplug::api::bleuuid::uuid_from_u16(0x3081),
    btleplug::api::bleuuid::uuid_from_u16(0x3082),
    btleplug::api::bleuuid::uuid_from_u16(0x3083),
];
/// TX notify — device → host (data stream). UUIDs from firmware
/// `serial_service_uuid.inc`: TX ends in `61fe`, RX ends in `62fe`.
pub const TX_CHAR: Uuid = uuid!("19ed82ae-ed21-4c9d-4145-228e61fe0000");
/// RX write — host → device (data stream).
pub const RX_CHAR: Uuid = uuid!("19ed82ae-ed21-4c9d-4145-228e62fe0000");
/// FLOW_CTRL notify/read — free bytes currently available in the firmware's
/// RX stream buffer, little-endian (width varies by firmware: u16 on older
/// builds, u32 on newer). Do NOT invert the meaning: value = free space, not
/// bytes pending. Decrement locally after each write; overwrite on notification.
pub const OVERFLOW_CHAR: Uuid = uuid!("19ed82ae-ed21-4c9d-4145-228e63fe0000");
/// RPC_STATE notify — session state changes (non-fatal, informational).
pub const RPC_STATE_CHAR: Uuid = uuid!("19ed82ae-ed21-4c9d-4145-228e64fe0000");

/// Advertised name prefix we filter on when scanning.
pub const FLIPPER_NAME_PREFIX: &str = "Flipper ";
