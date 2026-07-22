use std::collections::BTreeMap;

use serde::Serialize;
use thiserror::Error;

/// Stable error shape returned across the Tauri IPC boundary.
///
/// `FlipperError` remains the internal domain error. Its custom `Serialize`
/// implementation converts it to this deliberately small, safe envelope only
/// when Tauri sends an error to the webview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandError {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
    pub operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Error)]
pub enum FlipperError {
    #[error("Serial port error: {0}")]
    Serial(#[from] serialport::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Protobuf decode error: {0}")]
    Decode(#[from] prost::DecodeError),

    #[error("Protobuf encode error: {0}")]
    Encode(#[from] prost::EncodeError),

    #[error("Device not connected")]
    NotConnected,

    #[error("RPC error (status={status}) on command {command_id}")]
    Rpc { status: i32, command_id: u32 },

    #[error("Timeout waiting for device response")]
    Timeout,

    #[error("Unexpected response from device")]
    UnexpectedResponse,

    #[error("Session startup failed: {0}")]
    Session(String),

    #[error("Device is in CLI mode — disconnect terminal first")]
    CliModeActive,

    #[error(
        "BLE pairing required — pair the Flipper in your OS Bluetooth settings and try again ({0})"
    )]
    BlePairingRequired(String),

    #[error("Operation not supported over BLE — connect via USB")]
    BleUnsupported,

    #[error("Device capability handshake is invalid for {key}: {reason}")]
    InvalidHandshake { key: String, reason: String },

    #[error("Transfer cancelled")]
    TransferCancelled,

    #[error("Connection is busy; try the operation again")]
    ConnectionBusy,

    #[error("Connection work is locked while the connection is {current}")]
    ConnectionLocked { current: String },

    #[error("Connection protocol error: {message}")]
    ConnectionProtocol {
        message: String,
        command_id: Option<u32>,
    },

    #[error("Connection timed out: {message}")]
    ConnectionTimeout {
        message: String,
        command_id: Option<u32>,
    },

    #[error("Connection failed and must be re-established: {0}")]
    ConnectionFatal(String),

    #[error("Invalid command input: {0}")]
    InvalidInput(String),

    #[error("Invalid device path: {reason}")]
    InvalidDevicePath { path: String, reason: String },

    #[error("Internal error: {0}")]
    Internal(String),
}

impl FlipperError {
    /// Convert a domain error to the public, version-stable IPC contract.
    pub fn command_error(&self) -> CommandError {
        let mut details = BTreeMap::new();
        let mut path = None;
        let (code, retryable, operation, command_id) = match self {
            Self::Serial(error) => {
                details.insert("kind".into(), format!("{:?}", error.kind()));
                ("transport_error", true, "transport", None)
            }
            Self::Io(error) => {
                details.insert("kind".into(), format!("{:?}", error.kind()));
                (
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) {
                        "timeout"
                    } else {
                        "transport_error"
                    },
                    true,
                    "transport",
                    None,
                )
            }
            Self::Decode(_) | Self::Encode(_) => ("protocol_error", false, "protocol", None),
            Self::NotConnected => ("not_connected", true, "connection", None),
            Self::Rpc { status, command_id } => {
                details.insert("status".into(), status.to_string());
                ("rpc_error", false, "rpc", Some(*command_id))
            }
            Self::Timeout => ("timeout", true, "rpc", None),
            Self::UnexpectedResponse => ("protocol_error", false, "rpc", None),
            Self::Session(_) => ("session_error", false, "session", None),
            Self::CliModeActive => {
                details.insert("currentMode".into(), "cli".into());
                ("operation_locked", true, "connection", None)
            }
            Self::BlePairingRequired(_) => ("ble_pairing_required", true, "connection", None),
            Self::BleUnsupported => ("unsupported", false, "connection", None),
            Self::InvalidHandshake { key, .. } => {
                details.insert("key".into(), key.clone());
                ("invalid_handshake", false, "handshake", None)
            }
            Self::TransferCancelled => ("cancelled", false, "transfer", None),
            Self::ConnectionBusy => ("busy", true, "connection", None),
            Self::ConnectionLocked { current } => {
                details.insert("currentMode".into(), current.clone());
                ("operation_locked", true, "connection", None)
            }
            Self::ConnectionProtocol { command_id, .. } => {
                ("protocol_error", false, "protocol", *command_id)
            }
            Self::ConnectionTimeout { command_id, .. } => {
                ("timeout", true, "protocol", *command_id)
            }
            Self::ConnectionFatal(_) => ("connection_fatal", true, "connection", None),
            Self::InvalidInput(_) => ("invalid_argument", false, "input", None),
            Self::InvalidDevicePath {
                path: device_path, ..
            } => {
                path = Some(device_path.clone());
                ("invalid_path", false, "storage", None)
            }
            Self::Internal(_) => ("internal", false, "internal", None),
        };

        CommandError {
            code,
            message: self.to_string(),
            retryable,
            operation,
            command_id,
            path,
            details: (!details.is_empty()).then_some(details),
        }
    }
}

// `Serialize` is required so FlipperError can be returned from
// `#[tauri::command]`. Serialize the public envelope, never the display string.
impl Serialize for FlipperError {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        self.command_error().serialize(serializer)
    }
}

pub type Result<T> = std::result::Result<T, FlipperError>;

/// Whether an error means the byte stream can no longer be trusted.
///
/// Timeout-like I/O failures are intentionally retryable: both USB serial and
/// BLE use them for ordinary backpressure/deadline misses. Decode, encode, and
/// permanent transport failures require the connection owner to drop the
/// client so no later request can consume a desynchronized stream.
pub(crate) fn is_fatal_transport_error(error: &FlipperError) -> bool {
    match error {
        FlipperError::Serial(_) => true,
        FlipperError::Io(io) => !matches!(
            io.kind(),
            std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::Interrupted
                | std::io::ErrorKind::WouldBlock
        ),
        FlipperError::Decode(_) | FlipperError::Encode(_) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::FlipperError;

    #[test]
    fn ipc_error_serializes_as_stable_object_not_display_string() {
        let value = serde_json::to_value(FlipperError::ConnectionBusy).unwrap();
        assert_eq!(value["code"], "busy");
        assert_eq!(value["retryable"], true);
        assert_eq!(value["operation"], "connection");
        assert!(value["message"].as_str().unwrap().contains("busy"));
        assert!(value.get("commandId").is_none());
        assert!(value.get("path").is_none());
        assert!(value.get("details").is_none());
    }

    #[test]
    fn ipc_error_preserves_rpc_command_identity_and_safe_status() {
        let value = serde_json::to_value(FlipperError::Rpc {
            status: 7,
            command_id: 42,
        })
        .unwrap();
        assert_eq!(value["code"], "rpc_error");
        assert_eq!(value["commandId"], 42);
        assert_eq!(value["details"]["status"], "7");
    }

    #[test]
    fn cancellation_and_timeout_have_decision_safe_codes() {
        assert_eq!(
            FlipperError::TransferCancelled.command_error().code,
            "cancelled"
        );
        let timeout = FlipperError::ConnectionTimeout {
            message: "response deadline".into(),
            command_id: Some(9),
        }
        .command_error();
        assert_eq!(timeout.code, "timeout");
        assert!(timeout.retryable);
        assert_eq!(timeout.command_id, Some(9));
    }

    #[test]
    fn typed_device_path_error_populates_only_safe_path_metadata() {
        let value = serde_json::to_value(FlipperError::InvalidDevicePath {
            path: "/ext/example.txt".into(),
            reason: "path contains traversal".into(),
        })
        .unwrap();
        assert_eq!(value["code"], "invalid_path");
        assert_eq!(value["operation"], "storage");
        assert_eq!(value["path"], "/ext/example.txt");
        assert!(value.get("details").is_none());
    }
}
