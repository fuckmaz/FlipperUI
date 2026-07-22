use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Emitter, State};

use crate::commands::client::{
    connection_handle, execute_connection, retire_connection_owner, with_connection,
};
use crate::error::{FlipperError, Result};
use crate::flipper::diag;
use crate::flipper::gpio;
use crate::pb_gpio;
use crate::state::AppState;

// Re-export the snapshot types so the frontend's generated bindings (and
// `tauri::generate_handler!`) can find them through the commands module.
pub use crate::flipper::gpio::{GpioPinSnapshot, GpioSnapshot};

const MIN_PULSE_DURATION_MS: u32 = 1;
const MAX_PULSE_DURATION_MS: u32 = 5_000;

/// Map a user-supplied pin name (`"PC0"`, `"PA7"`, …) to the proto enum.
/// Case-insensitive: the canonical names are uppercase, but accept any case so
/// the frontend doesn't have to be strict. Unknown pins surface as a
/// `Session` error — that's the closest fit in `FlipperError` for "invalid
/// argument from the caller", matches the pattern used by
/// `commands::device::validate_range`, and serializes to a clean string for
/// the JS side.
fn parse_pin(s: &str) -> Result<pb_gpio::GpioPin> {
    pb_gpio::GpioPin::from_str_name(&s.to_ascii_uppercase()).ok_or_else(|| {
        FlipperError::Session(format!(
            "Invalid GPIO pin '{s}': expected one of PC0, PC1, PC3, PB2, PB3, PA4, PA6, PA7"
        ))
    })
}

/// Map `"input"`/`"output"` (any case) to the proto enum.
fn parse_mode(s: &str) -> Result<pb_gpio::GpioPinMode> {
    match s.to_ascii_lowercase().as_str() {
        "input" => Ok(pb_gpio::GpioPinMode::Input),
        "output" => Ok(pb_gpio::GpioPinMode::Output),
        _ => Err(FlipperError::Session(format!(
            "Invalid GPIO mode '{s}': expected 'input' or 'output'"
        ))),
    }
}

/// Map `"no"`/`"up"`/`"down"` (any case) to the proto enum.
fn parse_pull(s: &str) -> Result<pb_gpio::GpioInputPull> {
    match s.to_ascii_lowercase().as_str() {
        "no" => Ok(pb_gpio::GpioInputPull::No),
        "up" => Ok(pb_gpio::GpioInputPull::Up),
        "down" => Ok(pb_gpio::GpioInputPull::Down),
        _ => Err(FlipperError::Session(format!(
            "Invalid GPIO pull '{s}': expected 'no', 'up', or 'down'"
        ))),
    }
}

/// GPIO writes are digital at the Tauri boundary. Do not silently forward an
/// arbitrary `u8` to the firmware's wider `uint32` field.
fn parse_output_value(value: u8) -> Result<u32> {
    match value {
        0 | 1 => Ok(u32::from(value)),
        _ => Err(FlipperError::Session(format!(
            "Invalid GPIO value '{value}': expected 0 or 1"
        ))),
    }
}

fn parse_pulse_duration(duration_ms: u32) -> Result<Duration> {
    if !(MIN_PULSE_DURATION_MS..=MAX_PULSE_DURATION_MS).contains(&duration_ms) {
        return Err(FlipperError::Session(format!(
            "Invalid GPIO pulse duration '{duration_ms} ms': expected {MIN_PULSE_DURATION_MS}–{MAX_PULSE_DURATION_MS} ms"
        )));
    }
    Ok(Duration::from_millis(u64::from(duration_ms)))
}

fn pulse_failure_requires_disconnect(failure: &gpio::GpioPulseFailure) -> bool {
    failure.pin_state == gpio::GpioPulsePinState::Indeterminate
        || crate::error::is_fatal_transport_error(&failure.primary_error)
        || failure
            .low_errors
            .iter()
            .any(crate::error::is_fatal_transport_error)
}

fn pulse_failure_message(pin: pb_gpio::GpioPin, failure: &gpio::GpioPulseFailure) -> String {
    let pin = pin.as_str_name();
    let low_errors = failure
        .low_errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");

    match (failure.phase, failure.pin_state) {
        (gpio::GpioPulsePhase::Preflight, _) => {
            format!("GPIO pulse preflight for {pin} failed: {}", failure.primary_error)
        }
        (gpio::GpioPulsePhase::DriveHigh, gpio::GpioPulsePinState::LowConfirmed) => {
            let cleanup = if low_errors.is_empty() {
                "LOW cleanup was confirmed".to_string()
            } else {
                format!("the initial LOW cleanup failed ({low_errors}), but a LOW retry was confirmed")
            };
            format!(
                "GPIO pulse on {pin} failed while driving HIGH, but {cleanup}: {}",
                failure.primary_error
            )
        }
        (gpio::GpioPulsePhase::DriveLow, gpio::GpioPulsePinState::LowConfirmed) => format!(
            "GPIO pulse on {pin} encountered an error while driving LOW, but an idempotent LOW retry was confirmed: {}",
            failure.primary_error
        ),
        (_, gpio::GpioPulsePinState::Indeterminate) => format!(
            "GPIO pulse on {pin} failed and LOW could not be confirmed; pin state is indeterminate. Primary error: {}. LOW errors: {low_errors}",
            failure.primary_error
        ),
        // The only Unchanged failure is currently preflight. Keep a defensive
        // fallback so a future sequence phase still produces useful context.
        (_, gpio::GpioPulsePinState::Unchanged) => {
            format!("GPIO pulse on {pin} failed before changing the pin: {}", failure.primary_error)
        }
    }
}

/// Snapshot every GPIO pin's mode + value plus the OTG flag. Issues N+1 RPC
/// calls sequentially under a single actor job so other commands don't
/// observe the device mid-walk.
#[tauri::command]
pub async fn gpio_snapshot(state: State<'_, AppState>) -> Result<GpioSnapshot> {
    with_connection(Arc::clone(&state.connection_owner), gpio::snapshot).await
}

#[tauri::command]
pub async fn gpio_set_mode(pin: String, mode: String, state: State<'_, AppState>) -> Result<()> {
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        let pin = parse_pin(&pin)?;
        let mode = parse_mode(&mode)?;
        gpio::set_pin_mode(client, pin, mode)
    })
    .await
}

#[tauri::command]
pub async fn gpio_get_mode(pin: String, state: State<'_, AppState>) -> Result<String> {
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        let pin = parse_pin(&pin)?;
        let mode = gpio::get_pin_mode(client, pin)?;
        Ok(match mode {
            pb_gpio::GpioPinMode::Input => "input".to_string(),
            pb_gpio::GpioPinMode::Output => "output".to_string(),
        })
    })
    .await
}

#[tauri::command]
pub async fn gpio_set_pull(pin: String, pull: String, state: State<'_, AppState>) -> Result<()> {
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        let pin = parse_pin(&pin)?;
        let pull = parse_pull(&pull)?;
        gpio::set_input_pull(client, pin, pull)
    })
    .await
}

#[tauri::command]
pub async fn gpio_read_pin(pin: String, state: State<'_, AppState>) -> Result<u8> {
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        let pin = parse_pin(&pin)?;
        let value = gpio::read_pin(client, pin)?;
        // The firmware returns a raw uint32 sample — clamp to 0/1 so the JS
        // side sees a tidy boolean-ish u8 matching the snapshot encoding.
        Ok(if value == 0 { 0u8 } else { 1u8 })
    })
    .await
}

#[tauri::command]
pub async fn gpio_write_pin(pin: String, value: u8, state: State<'_, AppState>) -> Result<()> {
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        let pin = parse_pin(&pin)?;
        let value = parse_output_value(value)?;
        gpio::write_pin(client, pin, value)
    })
    .await
}

/// Pulse one output pin HIGH and then LOW while retaining exclusive access to
/// the connected client for the complete sequence. A normal app disconnect or
/// component unmount therefore cannot interrupt the cleanup write.
#[tauri::command]
pub async fn gpio_pulse_pin(
    pin: String,
    duration_ms: u32,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<()> {
    let pin = parse_pin(&pin)?;
    let duration = parse_pulse_duration(duration_ms)?;
    let owner = Arc::clone(&state.connection_owner);
    let handle = connection_handle(&owner)?;
    let outcome = execute_connection(&handle, move |client| {
        Ok(gpio::pulse_pin(client, pin, duration))
    })
    .await?;

    let failure = match outcome {
        Ok(()) => return Ok(()),
        Err(failure) => failure,
    };
    let reason = pulse_failure_message(pin, &failure);
    let disconnect = pulse_failure_requires_disconnect(&failure);
    diag::log_event("GpioPulseFailed", reason.clone());

    if disconnect {
        tracing::warn!("tearing down connection after GPIO pulse failure: {reason}");
        diag::log_event("GpioPulseConnectionTornDown", reason.clone());
        let lifecycle = Arc::clone(&state.connection_lifecycle);
        let _lifecycle = lifecycle.lock().await;
        if retire_connection_owner(&owner, &handle) {
            let _ = handle.shutdown().await;
            if let Some(cancel) = state
                .ble_cancel_tx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                let _ = cancel.send(());
            }
            let _ = app.emit("flipper-disconnected", &reason);
        }
    } else {
        tracing::warn!("GPIO pulse failed with connection retained: {reason}");
    }

    Err(FlipperError::Session(reason))
}

#[tauri::command]
pub async fn gpio_get_otg(state: State<'_, AppState>) -> Result<bool> {
    with_connection(Arc::clone(&state.connection_owner), |client| {
        let mode = gpio::get_otg_mode(client)?;
        Ok(matches!(mode, pb_gpio::GpioOtgMode::On))
    })
    .await
}

#[tauri::command]
pub async fn gpio_set_otg(on: bool, state: State<'_, AppState>) -> Result<()> {
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        let mode = if on {
            pb_gpio::GpioOtgMode::On
        } else {
            pb_gpio::GpioOtgMode::Off
        };
        gpio::set_otg_mode(client, mode)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pin_accepts_canonical_names() {
        assert_eq!(parse_pin("PC0").unwrap(), pb_gpio::GpioPin::Pc0);
        assert_eq!(parse_pin("PA7").unwrap(), pb_gpio::GpioPin::Pa7);
    }

    #[test]
    fn parse_pin_is_case_insensitive() {
        assert_eq!(parse_pin("pc0").unwrap(), pb_gpio::GpioPin::Pc0);
        assert_eq!(parse_pin("Pa7").unwrap(), pb_gpio::GpioPin::Pa7);
    }

    #[test]
    fn parse_pin_rejects_unknown() {
        assert!(parse_pin("PD0").is_err());
        assert!(parse_pin("").is_err());
    }

    #[test]
    fn parse_mode_is_case_insensitive() {
        assert_eq!(parse_mode("INPUT").unwrap(), pb_gpio::GpioPinMode::Input);
        assert_eq!(parse_mode("input").unwrap(), pb_gpio::GpioPinMode::Input);
        assert_eq!(parse_mode("Output").unwrap(), pb_gpio::GpioPinMode::Output);
        assert!(parse_mode("floating").is_err());
    }

    #[test]
    fn parse_pull_accepts_all_three() {
        assert_eq!(parse_pull("no").unwrap(), pb_gpio::GpioInputPull::No);
        assert_eq!(parse_pull("UP").unwrap(), pb_gpio::GpioInputPull::Up);
        assert_eq!(parse_pull("Down").unwrap(), pb_gpio::GpioInputPull::Down);
        assert!(parse_pull("sideways").is_err());
    }

    #[test]
    fn parse_output_value_accepts_only_binary_values() {
        assert_eq!(parse_output_value(0).unwrap(), 0);
        assert_eq!(parse_output_value(1).unwrap(), 1);
        assert!(parse_output_value(2).is_err());
        assert!(parse_output_value(u8::MAX).is_err());
    }

    #[test]
    fn parse_pulse_duration_accepts_inclusive_bounds() {
        assert_eq!(
            parse_pulse_duration(MIN_PULSE_DURATION_MS).unwrap(),
            Duration::from_millis(u64::from(MIN_PULSE_DURATION_MS))
        );
        assert_eq!(
            parse_pulse_duration(MAX_PULSE_DURATION_MS).unwrap(),
            Duration::from_millis(u64::from(MAX_PULSE_DURATION_MS))
        );
    }

    #[test]
    fn parse_pulse_duration_rejects_values_outside_bounds() {
        assert!(parse_pulse_duration(MIN_PULSE_DURATION_MS - 1).is_err());
        assert!(parse_pulse_duration(MAX_PULSE_DURATION_MS + 1).is_err());
    }

    fn pulse_failure(
        pin_state: gpio::GpioPulsePinState,
        primary_error: FlipperError,
    ) -> gpio::GpioPulseFailure {
        gpio::GpioPulseFailure {
            phase: gpio::GpioPulsePhase::DriveLow,
            pin_state,
            primary_error,
            low_errors: Vec::new(),
        }
    }

    #[test]
    fn indeterminate_pin_state_always_requires_disconnect() {
        let failure = pulse_failure(
            gpio::GpioPulsePinState::Indeterminate,
            FlipperError::Timeout,
        );
        assert!(pulse_failure_requires_disconnect(&failure));
    }

    #[test]
    fn confirmed_low_with_transient_error_retains_connection() {
        let failure = pulse_failure(gpio::GpioPulsePinState::LowConfirmed, FlipperError::Timeout);
        assert!(!pulse_failure_requires_disconnect(&failure));
    }

    #[test]
    fn fatal_transport_error_requires_disconnect_even_when_low_is_confirmed() {
        let failure = pulse_failure(
            gpio::GpioPulsePinState::LowConfirmed,
            FlipperError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "transport closed",
            )),
        );
        assert!(pulse_failure_requires_disconnect(&failure));
    }

    #[test]
    fn indeterminate_message_identifies_pin_and_unknown_state() {
        let mut failure = pulse_failure(
            gpio::GpioPulsePinState::Indeterminate,
            FlipperError::Timeout,
        );
        failure.low_errors.push(FlipperError::Timeout);

        let message = pulse_failure_message(pb_gpio::GpioPin::Pa7, &failure);
        assert!(message.contains("PA7"));
        assert!(message.contains("LOW could not be confirmed"));
        assert!(message.contains("pin state is indeterminate"));
    }
}
