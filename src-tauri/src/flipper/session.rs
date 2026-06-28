use std::collections::HashMap;

use crate::error::{FlipperError, Result};
use crate::flipper::client::FlipperClient;
use crate::flipper::framing::{read_response, write_message};
use crate::flipper::transport::SerialTransport;
use crate::flipper::{SERIAL_TIMEOUT_DRAIN, SERIAL_TIMEOUT_NORMAL};
use crate::pb;
use crate::pb::main::Content;
use crate::pb_system;

const SESSION_CMD: &[u8] = b"start_rpc_session\r";

/// Open a serial port, perform the RPC session handshake, and return a connected client.
pub fn open_session(port_name: &str) -> Result<FlipperClient> {
    tracing::info!(port = %port_name, "open_session: opening serial port @230400");
    let mut port = serialport::new(port_name, 230400)
        .timeout(SERIAL_TIMEOUT_DRAIN)
        .open()
        .map_err(|e| {
            tracing::warn!(port = %port_name, error = %e, "open_session: serial open failed");
            e
        })?;

    // Windows' usbser.sys doesn't auto-assert DTR/RTS on open, and the Flipper
    // firmware gates the CLI on DTR — without this the `start_rpc_session\r`
    // write completes but no ack ever comes back and the read times out as
    // OS error 121 (ERROR_SEM_TIMEOUT). macOS/Linux don't need this but the
    // call is harmless there. Both lines are best-effort: log on failure and
    // keep going, since some virtual COM stacks reject these calls outright.
    if let Err(e) = port.write_data_terminal_ready(true) {
        tracing::warn!(port = %port_name, error = %e, "open_session: DTR assert failed (continuing)");
    }
    if let Err(e) = port.write_request_to_send(true) {
        tracing::warn!(port = %port_name, error = %e, "open_session: RTS assert failed (continuing)");
    }

    let mut transport: Box<dyn crate::flipper::transport::Transport> =
        Box::new(SerialTransport::new(port));

    // Drain any pending CLI text (prompt, log lines, etc.)
    let mut drain_buf = [0u8; 256];
    let mut drained = 0usize;
    loop {
        match transport.read(&mut drain_buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => drained += n,
        }
    }
    tracing::info!(port = %port_name, drained, "open_session: drained pre-session bytes");

    // Switch to normal timeout for the session handshake
    transport.set_timeout(SERIAL_TIMEOUT_NORMAL).map_err(|e| {
        tracing::warn!(port = %port_name, error = %e, "open_session: set_timeout failed");
        e
    })?;

    // Send RPC session start command (CR only — the Flipper CLI expects \r not \r\n)
    transport.write_all(SESSION_CMD).map_err(|e| {
        tracing::warn!(port = %port_name, error = %e, raw_os = ?e.raw_os_error(), "open_session: write SESSION_CMD failed");
        e
    })?;
    transport.flush().map_err(|e| {
        tracing::warn!(port = %port_name, error = %e, raw_os = ?e.raw_os_error(), "open_session: flush after SESSION_CMD failed");
        e
    })?;
    tracing::info!(port = %port_name, "open_session: sent start_rpc_session, awaiting ack");

    // Wait for the device to acknowledge with a newline.
    // The transport already has SERIAL_TIMEOUT_NORMAL set, so read() will return
    // TimedOut if no bytes arrive within 5 seconds — no manual deadline needed.
    // Windows surfaces the same condition as ERROR_SEM_TIMEOUT (OS error 121),
    // which Rust's std doesn't map to ErrorKind::TimedOut, so we treat it as a
    // timeout explicitly.
    let mut byte = [0u8; 1];
    loop {
        match transport.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => break,
            Ok(_) => {} // consume echoed characters, \r, etc.
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut || e.raw_os_error() == Some(121) => {
                tracing::warn!(
                    port = %port_name,
                    raw_os = ?e.raw_os_error(),
                    "open_session: timed out waiting for RPC session ack",
                );
                return Err(FlipperError::Session(
                    "Timed out waiting for RPC session acknowledgment".into(),
                ));
            }
            Err(e) => {
                tracing::warn!(port = %port_name, error = %e, raw_os = ?e.raw_os_error(), "open_session: read error during ack");
                return Err(e.into());
            }
        }
    }

    tracing::info!(port = %port_name, "open_session: RPC session established");
    Ok(FlipperClient::new(transport))
}

/// Send a ping and verify the device responds correctly.
pub fn ping(client: &mut FlipperClient) -> Result<()> {
    let id = client.next_command_id();
    let req = pb::Main {
        command_id: id,
        command_status: 0,
        has_next: false,
        content: Some(Content::SystemPingRequest(pb_system::PingRequest {
            data: vec![0xDE, 0xAD, 0xBE, 0xEF],
        })),
    };
    write_message(&mut *client.transport, &req)?;
    let resp = read_response(&mut *client.transport)?;
    check_response(&resp, id)?;
    Ok(())
}

/// Retrieve device info key-value pairs from the Flipper.
pub fn get_device_info(client: &mut FlipperClient) -> Result<HashMap<String, String>> {
    let id = client.next_command_id();
    let req = pb::Main {
        command_id: id,
        command_status: 0,
        has_next: false,
        content: Some(Content::SystemDeviceInfoRequest(
            pb_system::DeviceInfoRequest {},
        )),
    };
    write_message(&mut *client.transport, &req)?;

    let mut info = HashMap::new();
    loop {
        let msg = read_response(&mut *client.transport)?;
        check_response(&msg, id)?;
        if let Some(Content::SystemDeviceInfoResponse(r)) = msg.content {
            info.insert(r.key, r.value);
        }
        if !msg.has_next {
            break;
        }
    }
    Ok(info)
}

/// Get power/battery info from the Flipper. Returns key-value pairs
/// (e.g. "charge", "health", "voltage", "current", "temperature").
pub fn get_power_info(client: &mut FlipperClient) -> Result<HashMap<String, String>> {
    let id = client.next_command_id();
    let req = pb::Main {
        command_id: id,
        command_status: 0,
        has_next: false,
        content: Some(Content::SystemPowerInfoRequest(
            pb_system::PowerInfoRequest {},
        )),
    };
    write_message(&mut *client.transport, &req)?;

    let mut info = HashMap::new();
    loop {
        let msg = read_response(&mut *client.transport)?;
        check_response(&msg, id)?;
        if let Some(Content::SystemPowerInfoResponse(r)) = msg.content {
            info.insert(r.key, r.value);
        }
        if !msg.has_next {
            break;
        }
    }
    Ok(info)
}

/// Set the Flipper's RTC to the supplied local date/time.
pub fn set_datetime(client: &mut FlipperClient, datetime: pb_system::DateTime) -> Result<()> {
    let id = client.next_command_id();
    let req = pb::Main {
        command_id: id,
        command_status: 0,
        has_next: false,
        content: Some(Content::SystemSetDatetimeRequest(
            pb_system::SetDateTimeRequest {
                datetime: Some(datetime),
            },
        )),
    };
    write_message(&mut *client.transport, &req)?;
    let resp = read_response(&mut *client.transport)?;
    check_response(&resp, id)?;
    Ok(())
}

/// Reboot the Flipper Zero.
/// mode: 0 = OS (normal), 1 = DFU, 2 = UPDATE
pub fn reboot(client: &mut FlipperClient, mode: i32) -> Result<()> {
    let id = client.next_command_id();
    let req = pb::Main {
        command_id: id,
        command_status: 0,
        has_next: false,
        content: Some(Content::SystemRebootRequest(pb_system::RebootRequest {
            mode,
        })),
    };
    write_message(&mut *client.transport, &req)?;
    // Device reboots immediately — no response expected.
    // The serial port will disconnect.
    Ok(())
}

/// Ask the Flipper to stage a firmware update from an already-uploaded bundle.
///
/// `manifest` is the absolute on-device path to the update manifest, e.g.
/// `/ext/update/f7-update-1.4.3/update.fuf`. The firmware validates the bundle
/// and replies with an `UpdateResponse` carrying a result code (0 = OK); the
/// caller maps non-zero codes to a human message. The device does **not** start
/// applying anything here — that happens on the subsequent `reboot(UPDATE)`.
///
/// Returns the raw `UpdateResultCode` (0 = OK). Older firmware may answer with
/// an `Empty` body on success, which we treat as code 0.
pub fn request_update(client: &mut FlipperClient, manifest: &str) -> Result<i32> {
    let id = client.next_command_id();
    let req = pb::Main {
        command_id: id,
        command_status: 0,
        has_next: false,
        content: Some(Content::SystemUpdateRequest(pb_system::UpdateRequest {
            update_manifest: manifest.to_string(),
        })),
    };
    write_message(&mut *client.transport, &req)?;
    let resp = read_response(&mut *client.transport)?;
    check_response(&resp, id)?;
    match resp.content {
        Some(Content::SystemUpdateResponse(r)) => Ok(r.code),
        _ => Ok(0),
    }
}

/// Validate that a response belongs to the expected command and has OK status.
/// Every RPC call site should use this instead of bare status checks.
pub fn check_response(msg: &pb::Main, expected_id: u32) -> Result<()> {
    if msg.command_id != expected_id {
        return Err(FlipperError::UnexpectedResponse);
    }
    if msg.command_status != 0 {
        return Err(FlipperError::Rpc {
            status: msg.command_status,
            command_id: expected_id,
        });
    }
    Ok(())
}
