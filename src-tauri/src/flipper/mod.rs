pub mod app;
pub mod apps;
pub mod badusb;
pub mod ble;
pub mod cli;
pub mod client;
pub mod diag;
pub mod fap_icon;
pub mod firmware;
pub mod framing;
pub mod gpio;
pub mod gui;
pub mod infrared;
pub mod library_prewalk;
pub mod library_walk;
pub mod nfc;
pub mod rfid;
pub mod session;
pub mod storage;
pub mod subghz;
pub mod transport;

use std::time::Duration;

/// Normal serial port timeout for RPC commands.
pub const SERIAL_TIMEOUT_NORMAL: Duration = Duration::from_secs(5);
/// Short timeout for draining leftover bytes from the serial port.
pub const SERIAL_TIMEOUT_DRAIN: Duration = Duration::from_millis(200);
/// Screen reader timeout on serial USB. Kept short so the reader releases
/// the client mutex frequently between frames — serial reads block on the
/// OS layer for the full timeout, freezing other RPC commands otherwise.
pub const SERIAL_TIMEOUT_SCREEN: Duration = Duration::from_millis(100);
/// Screen reader timeout on BLE. A 1024-byte body arrives over ~7 notifications
/// at ~15 ms connection intervals (~105 ms), so the USB-tuned 100 ms timeout
/// straddles every body read: it expires mid-frame, pushes the varint back,
/// and retries — burning a full timeout slice per frame. After an input write
/// adds latency, the thrashing compounds and the reader stops draining frames
/// in time. BLE waits on the RxBuffer condvar (not an OS read), the client
/// mutex isn't contended during streaming (input events route through a
/// channel), so a longer timeout costs nothing and lets each body read
/// complete in one shot. 500 ms gives ~5 frame intervals of margin while
/// bounding input-event lag if the firmware briefly pauses streaming.
pub const BLE_TIMEOUT_SCREEN: Duration = Duration::from_millis(500);
