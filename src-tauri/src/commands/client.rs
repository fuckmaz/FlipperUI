use std::sync::{Arc, Mutex};

use crate::error::{FlipperError, Result};
use crate::flipper::client::FlipperClient;
use crate::state::ConnectionMode;

/// Run a closure with exclusive access to the connected FlipperClient.
/// Rejects the call if the device is in CLI mode.
///
/// Errors are split into two classes:
/// - **Fatal** (transport/framing-level): Serial/Io/Decode/Encode. The wire
///   state is unrecoverable, so the client is dropped and the user must
///   reconnect. The screen reader (if running) will pick up the cleared mutex
///   on its next iteration and emit `flipper-disconnected`.
/// - **Transient** (protocol-level): Rpc/Timeout/UnexpectedResponse/Session/
///   etc. The connection is still healthy; the request just didn't go through.
///   Surface the error to the caller without touching the client. This is
///   crucial during an active screen stream — a single failed `power_info` or
///   `storage_info` poll used to nuke the entire connection and the stream.
pub fn with_client<T>(
    mode_mutex: &Arc<Mutex<ConnectionMode>>,
    client_mutex: &Arc<Mutex<Option<FlipperClient>>>,
    f: impl FnOnce(&mut FlipperClient) -> Result<T>,
) -> Result<T> {
    {
        let mode = mode_mutex.lock().unwrap();
        if *mode == ConnectionMode::Cli {
            return Err(FlipperError::CliModeActive);
        }
    }
    let mut guard = client_mutex.lock().unwrap();
    let client = guard.as_mut().ok_or(FlipperError::NotConnected)?;
    match f(client) {
        Ok(v) => Ok(v),
        Err(e) => {
            if is_fatal_transport_error(&e) {
                tracing::warn!("with_client tearing down connection: {e}");
                *guard = None;
            } else {
                tracing::debug!("with_client transient error (connection kept): {e}");
            }
            Err(e)
        }
    }
}

/// True for errors that mean the byte-stream/framing is unrecoverable.
/// Anything else (RPC status errors, timeouts, validation) leaves the
/// connection healthy.
///
/// BLE flow-control and BLE write timeouts surface here as `Io(TimedOut)` —
/// they mean the firmware momentarily fell behind, not that the link is dead.
/// Treating them as fatal (as past versions did) tore down the connection
/// mid-transfer on large uploads, so they are deliberately kept transient.
pub(crate) fn is_fatal_transport_error(e: &FlipperError) -> bool {
    match e {
        FlipperError::Serial(_) => true,
        FlipperError::Io(io) => match io.kind() {
            // Recoverable kinds — caller can retry without tearing the link
            // down. Everything else here is a true wire failure.
            std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock => false,
            _ => true,
        },
        FlipperError::Decode(_) | FlipperError::Encode(_) => true,
        _ => false,
    }
}
