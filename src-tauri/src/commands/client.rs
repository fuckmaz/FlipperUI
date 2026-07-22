use std::sync::{Arc, Mutex};

use crate::error::{is_fatal_transport_error, FlipperError, Result};
use crate::flipper::client::FlipperClient;
use crate::flipper::connection_actor::{
    ConnectionActorError, ConnectionHandle, ConnectionProtocolError, ConnectionState,
};

/// Map connection-owner failures at the command boundary while preserving
/// device-domain errors. All command modules use this one conversion so actor
/// admission/backpressure semantics cannot drift between features.
pub(crate) fn map_actor_error(error: ConnectionActorError) -> FlipperError {
    match error {
        ConnectionActorError::Device(error) if is_fatal_transport_error(&error) => {
            FlipperError::ConnectionFatal(error.to_string())
        }
        ConnectionActorError::Device(error) => error,
        ConnectionActorError::CliRequiresSerial => FlipperError::BleUnsupported,
        ConnectionActorError::Closed | ConnectionActorError::ActorStopped => {
            FlipperError::NotConnected
        }
        ConnectionActorError::ConnectionLost { cause } => {
            FlipperError::ConnectionFatal(cause.to_string())
        }
        ConnectionActorError::ModeRejected { current } => FlipperError::ConnectionLocked {
            current: current.to_string(),
        },
        ConnectionActorError::QueueFull => FlipperError::ConnectionBusy,
        ConnectionActorError::Protocol(protocol) => map_protocol_error(protocol),
        actor_error @ ConnectionActorError::ScreenStreamEndedDuringInput { command_id, .. } => {
            FlipperError::ConnectionProtocol {
                message: actor_error.to_string(),
                command_id: Some(command_id),
            }
        }
        invalid @ (ConnectionActorError::CliCommandBytesExceeded { .. }
        | ConnectionActorError::InvalidCliCommand
        | ConnectionActorError::InvalidScreenInputKey(_)
        | ConnectionActorError::InvalidScreenInputType(_)) => {
            FlipperError::InvalidInput(invalid.to_string())
        }
        actor_error @ ConnectionActorError::JobPanicked => {
            FlipperError::ConnectionFatal(actor_error.to_string())
        }
        actor_error @ ConnectionActorError::ThreadSpawn(_) => {
            FlipperError::Internal(actor_error.to_string())
        }
    }
}

fn map_protocol_error(error: ConnectionProtocolError) -> FlipperError {
    let command_id = match &error {
        ConnectionProtocolError::ResponseTimeout { command_id }
        | ConnectionProtocolError::ResponseReadDeadlineExceeded { command_id, .. }
        | ConnectionProtocolError::RequestBytesExceeded { command_id, .. }
        | ConnectionProtocolError::UnexpectedScreenResponse { command_id }
        | ConnectionProtocolError::UnexpectedCliStopResponse { command_id }
        | ConnectionProtocolError::UnexpectedCliPingResponse { command_id }
        | ConnectionProtocolError::TooManyResponseFrames { command_id, .. }
        | ConnectionProtocolError::ResponseBytesExceeded { command_id, .. } => Some(*command_id),
        ConnectionProtocolError::ForeignCommandId { expected_id, .. } => Some(*expected_id),
        ConnectionProtocolError::CliHandoffDeadlineExceeded { .. } => None,
    };
    let is_timeout = matches!(
        error,
        ConnectionProtocolError::ResponseTimeout { .. }
            | ConnectionProtocolError::ResponseReadDeadlineExceeded { .. }
            | ConnectionProtocolError::CliHandoffDeadlineExceeded { .. }
    );
    let message = error.to_string();
    if is_timeout {
        FlipperError::ConnectionTimeout {
            message,
            command_id,
        }
    } else {
        FlipperError::ConnectionProtocol {
            message,
            command_id,
        }
    }
}

/// Clone the live actor handle without granting access to the client or its
/// transport. Poison recovery is safe because the actor state remains the
/// source of truth and a stale handle rejects further work.
pub(crate) fn connection_handle(
    owner: &Arc<Mutex<Option<ConnectionHandle>>>,
) -> Result<ConnectionHandle> {
    let guard = match owner.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!("connection-owner slot was poisoned; recovering");
            poisoned.into_inner()
        }
    };
    let handle = guard.as_ref().cloned().ok_or(FlipperError::NotConnected)?;
    if handle.state() == ConnectionState::Disconnected {
        return Err(FlipperError::NotConnected);
    }
    Ok(handle)
}

/// Remove `expected` from the published owner slot only if it is still the
/// current connection. Fatal cleanup uses this identity check so a stale RPC
/// completion can never tear down a concurrently established replacement.
/// The caller that wins this claim owns the one global disconnect event.
pub(crate) fn retire_connection_owner(
    owner: &Mutex<Option<ConnectionHandle>>,
    expected: &ConnectionHandle,
) -> bool {
    let mut slot = match owner.lock() {
        Ok(slot) => slot,
        Err(poisoned) => {
            tracing::warn!("connection-owner slot was poisoned during retirement; recovering");
            poisoned.into_inner()
        }
    };
    if slot
        .as_ref()
        .is_some_and(|current| current.same_connection(expected))
    {
        slot.take();
        true
    } else {
        false
    }
}

/// Execute one operation through a previously captured actor identity.
/// Keeping that identity lets lifecycle-sensitive callers retire only the
/// connection on which their operation actually ran.
pub(crate) async fn execute_connection<T, F>(handle: &ConnectionHandle, work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&mut FlipperClient) -> Result<T> + Send + 'static,
{
    handle
        .execute_legacy_rpc(work)
        .await
        .map_err(map_actor_error)
}

/// Execute one blocking legacy operation on the dedicated connection-owner
/// thread. This is the migration seam for existing FlipperClient helpers;
/// bounded admission and atomic mode checks happen before the closure runs.
pub(crate) async fn with_connection<T, F>(
    owner: Arc<Mutex<Option<ConnectionHandle>>>,
    work: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&mut FlipperClient) -> Result<T> + Send + 'static,
{
    let handle = connection_handle(&owner)?;
    execute_connection(&handle, work).await
}

#[cfg(test)]
mod migration_tests {
    const DEVICE_SESSION: &str = include_str!("device.rs");
    const GUI_SCREEN: &str = include_str!("gui.rs");
    const STORAGE: &str = include_str!("storage.rs");
    const APPLICATIONS: &str = concat!(
        include_str!("app.rs"),
        include_str!("apps.rs"),
        include_str!("badusb.rs")
    );
    const GPIO: &str = include_str!("gpio.rs");
    const SIGNAL_ACTIONS: &str = concat!(
        include_str!("subghz.rs"),
        include_str!("infrared.rs"),
        include_str!("nfc.rs"),
        include_str!("rfid.rs")
    );
    const SCANS: &str = concat!(
        include_str!("library_prewalk.rs"),
        include_str!("apps.rs"),
        include_str!("badusb.rs"),
        include_str!("subghz.rs"),
        include_str!("infrared.rs"),
        include_str!("nfc.rs"),
        include_str!("rfid.rs")
    );
    const FIRMWARE_STAGING: &str = include_str!("firmware.rs");

    fn assert_no_legacy_connection_ownership(group: &str, source: &str) {
        for forbidden in [
            concat!("state", ".client"),
            concat!(".client", ".lock("),
            concat!("with_", "client("),
            concat!("screen_stream", "_active"),
            concat!("input_event", "_tx"),
        ] {
            assert!(
                !source.contains(forbidden),
                "{group} still contains forbidden legacy ownership marker {forbidden}"
            );
        }
    }

    #[test]
    fn every_p2b_command_group_routes_through_the_connection_actor() {
        let rpc_groups = [
            ("device/session", DEVICE_SESSION),
            ("storage", STORAGE),
            ("applications", APPLICATIONS),
            ("GPIO", GPIO),
            ("signal actions", SIGNAL_ACTIONS),
            ("scans", SCANS),
            ("firmware staging", FIRMWARE_STAGING),
        ];
        for (group, source) in rpc_groups {
            assert_no_legacy_connection_ownership(group, source);
            assert!(
                source.contains("with_connection(") || source.contains("execute_connection("),
                "{group} has no actor request boundary"
            );
        }

        assert_no_legacy_connection_ownership("GUI/screen", GUI_SCREEN);
        for actor_call in [
            ".start_screen_stream()",
            ".send_screen_input(",
            ".stop_screen_stream()",
        ] {
            assert!(
                GUI_SCREEN.contains(actor_call),
                "GUI/screen is missing actor call {actor_call}"
            );
        }
    }
}
