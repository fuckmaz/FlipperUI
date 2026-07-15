use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::error::{FlipperError, Result};
use crate::flipper::client::FlipperClient;
use crate::flipper::framing::{read_response, write_message};
use crate::flipper::session::check_response;
use crate::pb;
use crate::pb::main::Content;
use crate::pb_gpio;

/// Single-pin state snapshot returned by [`snapshot`]. `mode` is `"input"`,
/// `"output"`, or `"other"` when the firmware currently owns the pin in an
/// alternate/analog mode. `value` is only available for inputs: the Flipper
/// RPC protocol rejects reads from output and alternate-mode pins and does not
/// expose the output latch.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpioPinSnapshot {
    pub pin: String,
    pub mode: String,
    pub value: Option<u8>,
}

/// Full GPIO state snapshot: every pin's observable mode/value plus the current
/// OTG flag. A value read is only issued for pins confirmed to be inputs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpioSnapshot {
    pub pins: Vec<GpioPinSnapshot>,
    pub otg: bool,
}

/// Pins the Flipper Zero exposes on its GPIO header, in the order the proto
/// enum declares them. Used by [`snapshot`] to drive the per-pin queries.
pub const ALL_PINS: [pb_gpio::GpioPin; 8] = [
    pb_gpio::GpioPin::Pc0,
    pb_gpio::GpioPin::Pc1,
    pb_gpio::GpioPin::Pc3,
    pb_gpio::GpioPin::Pb2,
    pb_gpio::GpioPin::Pb3,
    pb_gpio::GpioPin::Pa4,
    pb_gpio::GpioPin::Pa6,
    pb_gpio::GpioPin::Pa7,
];

/// Configure a pin as input or output.
pub fn set_pin_mode(
    client: &mut FlipperClient,
    pin: pb_gpio::GpioPin,
    mode: pb_gpio::GpioPinMode,
) -> Result<()> {
    let id = client.next_command_id();
    let req = pb::Main {
        command_id: id,
        command_status: 0,
        has_next: false,
        content: Some(Content::GpioSetPinMode(pb_gpio::SetPinMode {
            pin: pin as i32,
            mode: mode as i32,
        })),
    };
    write_message(&mut *client.transport, &req)?;
    let resp = read_response(&mut *client.transport)?;
    check_response(&resp, id)?;
    Ok(())
}

/// Read the current mode (input/output) of a pin.
pub fn get_pin_mode(
    client: &mut FlipperClient,
    pin: pb_gpio::GpioPin,
) -> Result<pb_gpio::GpioPinMode> {
    let id = client.next_command_id();
    let req = pb::Main {
        command_id: id,
        command_status: 0,
        has_next: false,
        content: Some(Content::GpioGetPinMode(pb_gpio::GetPinMode {
            pin: pin as i32,
        })),
    };
    write_message(&mut *client.transport, &req)?;
    let resp = read_response(&mut *client.transport)?;
    check_response(&resp, id)?;
    match resp.content {
        Some(Content::GpioGetPinModeResponse(r)) => {
            pb_gpio::GpioPinMode::try_from(r.mode).map_err(|_| FlipperError::UnexpectedResponse)
        }
        _ => Err(FlipperError::UnexpectedResponse),
    }
}

/// Configure the input-pull (none / pull-up / pull-down) of an input pin.
pub fn set_input_pull(
    client: &mut FlipperClient,
    pin: pb_gpio::GpioPin,
    pull: pb_gpio::GpioInputPull,
) -> Result<()> {
    let id = client.next_command_id();
    let req = pb::Main {
        command_id: id,
        command_status: 0,
        has_next: false,
        content: Some(Content::GpioSetInputPull(pb_gpio::SetInputPull {
            pin: pin as i32,
            pull_mode: pull as i32,
        })),
    };
    write_message(&mut *client.transport, &req)?;
    let resp = read_response(&mut *client.transport)?;
    check_response(&resp, id)?;
    Ok(())
}

/// Read the digital value of a pin. Returns the raw `uint32` from the
/// firmware — callers normalise to 0/1 at the boundary.
pub fn read_pin(client: &mut FlipperClient, pin: pb_gpio::GpioPin) -> Result<u32> {
    let id = client.next_command_id();
    let req = pb::Main {
        command_id: id,
        command_status: 0,
        has_next: false,
        content: Some(Content::GpioReadPin(pb_gpio::ReadPin { pin: pin as i32 })),
    };
    write_message(&mut *client.transport, &req)?;
    let resp = read_response(&mut *client.transport)?;
    check_response(&resp, id)?;
    match resp.content {
        Some(Content::GpioReadPinResponse(r)) => Ok(r.value),
        _ => Err(FlipperError::UnexpectedResponse),
    }
}

/// Drive a digital value on a pin previously configured as output.
pub fn write_pin(client: &mut FlipperClient, pin: pb_gpio::GpioPin, value: u32) -> Result<()> {
    let id = client.next_command_id();
    let req = pb::Main {
        command_id: id,
        command_status: 0,
        has_next: false,
        content: Some(Content::GpioWritePin(pb_gpio::WritePin {
            pin: pin as i32,
            value,
        })),
    };
    write_message(&mut *client.transport, &req)?;
    let resp = read_response(&mut *client.transport)?;
    check_response(&resp, id)?;
    Ok(())
}

/// Drive an output pin high for `duration`, then drive it low again.
///
/// The caller holds exclusive access to the [`FlipperClient`] for the entire
/// function, so another command cannot disconnect or reuse the RPC transport
/// between the HIGH and LOW writes. If the HIGH write reports an error, a LOW
/// write is still attempted because the request may have reached the device
/// before the response failed.
pub fn pulse_pin(
    client: &mut FlipperClient,
    pin: pb_gpio::GpioPin,
    duration: Duration,
) -> std::result::Result<(), GpioPulseFailure> {
    let mode = get_pin_mode(client, pin).map_err(GpioPulseFailure::preflight)?;
    if mode != pb_gpio::GpioPinMode::Output {
        return Err(GpioPulseFailure::preflight(FlipperError::Session(format!(
            "GPIO pin {} must be configured as output before pulsing",
            pin.as_str_name()
        ))));
    }

    pulse_with(
        |value| write_pin(client, pin, value),
        std::thread::sleep,
        duration,
    )
}

/// Point in the pulse sequence at which an operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpioPulsePhase {
    Preflight,
    DriveHigh,
    DriveLow,
}

/// What the backend can safely say about the output after a failed pulse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpioPulsePinState {
    /// No HIGH request was sent, so this operation did not alter the pin.
    Unchanged,
    /// A LOW write received a successful acknowledgement.
    LowConfirmed,
    /// HIGH may have reached the device and no LOW acknowledgement was received.
    Indeterminate,
}

/// Retains the original transport errors and safety outcome so the Tauri
/// boundary can decide whether the whole connection must be torn down.
#[derive(Debug)]
pub struct GpioPulseFailure {
    pub phase: GpioPulsePhase,
    pub pin_state: GpioPulsePinState,
    pub primary_error: FlipperError,
    pub low_errors: Vec<FlipperError>,
}

impl GpioPulseFailure {
    fn preflight(error: FlipperError) -> Self {
        Self {
            phase: GpioPulsePhase::Preflight,
            pin_state: GpioPulsePinState::Unchanged,
            primary_error: error,
            low_errors: Vec::new(),
        }
    }
}

/// Testable HIGH/delay/LOW sequencing core for [`pulse_pin`].
fn pulse_with(
    mut write: impl FnMut(u32) -> Result<()>,
    sleep: impl FnOnce(Duration),
    duration: Duration,
) -> std::result::Result<(), GpioPulseFailure> {
    if let Err(high_error) = write(1) {
        // A failed response does not prove that the device failed to apply the
        // HIGH write. Make a best-effort LOW attempt and one idempotent retry
        // before returning the original error.
        let mut low_errors = Vec::with_capacity(2);
        for _ in 0..2 {
            match write(0) {
                Ok(()) => {
                    return Err(GpioPulseFailure {
                        phase: GpioPulsePhase::DriveHigh,
                        pin_state: GpioPulsePinState::LowConfirmed,
                        primary_error: high_error,
                        low_errors,
                    });
                }
                Err(error) => low_errors.push(error),
            }
        }
        return Err(GpioPulseFailure {
            phase: GpioPulsePhase::DriveHigh,
            pin_state: GpioPulsePinState::Indeterminate,
            primary_error: high_error,
            low_errors,
        });
    }

    sleep(duration);
    let low_error = match write(0) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };

    // LOW is idempotent. One retry can recover from a dropped acknowledgement
    // without extending the HIGH period more than the transport round-trip.
    match write(0) {
        Ok(()) => Err(GpioPulseFailure {
            phase: GpioPulsePhase::DriveLow,
            pin_state: GpioPulsePinState::LowConfirmed,
            primary_error: low_error,
            low_errors: Vec::new(),
        }),
        Err(retry_error) => Err(GpioPulseFailure {
            phase: GpioPulsePhase::DriveLow,
            pin_state: GpioPulsePinState::Indeterminate,
            primary_error: low_error,
            low_errors: vec![retry_error],
        }),
    }
}

/// Read the OTG power flag (5 V output on the GPIO header).
pub fn get_otg_mode(client: &mut FlipperClient) -> Result<pb_gpio::GpioOtgMode> {
    let id = client.next_command_id();
    let req = pb::Main {
        command_id: id,
        command_status: 0,
        has_next: false,
        content: Some(Content::GpioGetOtgMode(pb_gpio::GetOtgMode {})),
    };
    write_message(&mut *client.transport, &req)?;
    let resp = read_response(&mut *client.transport)?;
    check_response(&resp, id)?;
    match resp.content {
        Some(Content::GpioGetOtgModeResponse(r)) => {
            pb_gpio::GpioOtgMode::try_from(r.mode).map_err(|_| FlipperError::UnexpectedResponse)
        }
        _ => Err(FlipperError::UnexpectedResponse),
    }
}

/// Toggle OTG power.
pub fn set_otg_mode(client: &mut FlipperClient, mode: pb_gpio::GpioOtgMode) -> Result<()> {
    let id = client.next_command_id();
    let req = pb::Main {
        command_id: id,
        command_status: 0,
        has_next: false,
        content: Some(Content::GpioSetOtgMode(pb_gpio::SetOtgMode {
            mode: mode as i32,
        })),
    };
    write_message(&mut *client.transport, &req)?;
    let resp = read_response(&mut *client.transport)?;
    check_response(&resp, id)?;
    Ok(())
}

/// Convert the firmware's mode response into a mode the UI can display.
///
/// GPIO header pins commonly boot in alternate/analog modes. The firmware
/// reports that expected state as `ERROR_GPIO_UNKNOWN_PIN_MODE` (59) while also
/// embedding INPUT as a placeholder in the protobuf response. Treating it as a
/// fatal error made the entire GPIO page unusable on a freshly booted device;
/// treating it as INPUT would be worse because reads then fail with status 58.
fn observable_pin_mode(
    result: Result<pb_gpio::GpioPinMode>,
) -> Result<Option<pb_gpio::GpioPinMode>> {
    match result {
        Ok(mode) => Ok(Some(mode)),
        Err(FlipperError::Rpc { status: 59, .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Convenience that walks every pin and emits a [`GpioSnapshot`]. Sequential
/// by design: the firmware serialises RPC requests anyway and the call is
/// cheap. Reads are deliberately skipped for outputs and alternate-mode pins,
/// because firmware returns `ERROR_GPIO_MODE_INCORRECT` for those requests.
pub fn snapshot(client: &mut FlipperClient) -> Result<GpioSnapshot> {
    let mut pins = Vec::with_capacity(ALL_PINS.len());
    for pin in ALL_PINS {
        let mode = observable_pin_mode(get_pin_mode(client, pin))?;
        let value = if mode == Some(pb_gpio::GpioPinMode::Input) {
            let value = read_pin(client, pin)?;
            Some(if value == 0 { 0 } else { 1 })
        } else {
            None
        };
        pins.push(GpioPinSnapshot {
            pin: pin.as_str_name().to_string(),
            mode: match mode {
                Some(pb_gpio::GpioPinMode::Input) => "input".to_string(),
                Some(pb_gpio::GpioPinMode::Output) => "output".to_string(),
                None => "other".to_string(),
            },
            value,
        });
    }
    let otg = matches!(get_otg_mode(client)?, pb_gpio::GpioOtgMode::On);
    Ok(GpioSnapshot { pins, otg })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[test]
    fn unknown_firmware_pin_mode_is_an_observable_other_mode() {
        let result = observable_pin_mode(Err(FlipperError::Rpc {
            status: 59,
            command_id: 42,
        }))
        .unwrap();

        assert_eq!(result, None);
    }

    #[test]
    fn unexpected_mode_errors_are_not_hidden() {
        let error = observable_pin_mode(Err(FlipperError::Rpc {
            status: 58,
            command_id: 43,
        }))
        .unwrap_err();

        assert!(matches!(
            error,
            FlipperError::Rpc {
                status: 58,
                command_id: 43
            }
        ));
    }

    #[test]
    fn pulse_sequence_is_high_delay_low() {
        let events = RefCell::new(Vec::new());
        let duration = Duration::from_millis(100);

        pulse_with(
            |value| {
                events.borrow_mut().push(format!("write:{value}"));
                Ok(())
            },
            |actual| events.borrow_mut().push(format!("sleep:{actual:?}")),
            duration,
        )
        .unwrap();

        assert_eq!(events.into_inner(), ["write:1", "sleep:100ms", "write:0"]);
    }

    #[test]
    fn pulse_attempts_low_cleanup_when_high_write_fails() {
        let writes = RefCell::new(Vec::new());
        let slept = RefCell::new(false);

        let error = pulse_with(
            |value| {
                writes.borrow_mut().push(value);
                if value == 1 {
                    Err(FlipperError::Session("HIGH failed".to_string()))
                } else {
                    Ok(())
                }
            },
            |_| *slept.borrow_mut() = true,
            Duration::from_millis(100),
        )
        .unwrap_err();

        assert_eq!(writes.into_inner(), [1, 0]);
        assert!(!slept.into_inner());
        assert_eq!(error.phase, GpioPulsePhase::DriveHigh);
        assert_eq!(error.pin_state, GpioPulsePinState::LowConfirmed);
        assert_eq!(
            error.primary_error.to_string(),
            "Session startup failed: HIGH failed"
        );
    }

    #[test]
    fn pulse_retries_low_once_and_reports_confirmed_cleanup() {
        let writes = RefCell::new(Vec::new());
        let low_attempts = RefCell::new(0u8);

        let error = pulse_with(
            |value| {
                writes.borrow_mut().push(value);
                if value == 0 {
                    let mut attempts = low_attempts.borrow_mut();
                    *attempts += 1;
                    if *attempts == 1 {
                        return Err(FlipperError::Session("LOW failed".to_string()));
                    }
                    Ok(())
                } else {
                    Ok(())
                }
            },
            |_| {},
            Duration::from_millis(100),
        )
        .unwrap_err();

        assert_eq!(writes.into_inner(), [1, 0, 0]);
        assert_eq!(error.phase, GpioPulsePhase::DriveLow);
        assert_eq!(error.pin_state, GpioPulsePinState::LowConfirmed);
        assert_eq!(
            error.primary_error.to_string(),
            "Session startup failed: LOW failed"
        );
    }

    #[test]
    fn pulse_reports_indeterminate_when_both_low_attempts_fail() {
        let writes = RefCell::new(Vec::new());

        let error = pulse_with(
            |value| {
                writes.borrow_mut().push(value);
                if value == 0 {
                    Err(FlipperError::Session("LOW failed".to_string()))
                } else {
                    Ok(())
                }
            },
            |_| {},
            Duration::from_millis(100),
        )
        .unwrap_err();

        assert_eq!(writes.into_inner(), [1, 0, 0]);
        assert_eq!(error.phase, GpioPulsePhase::DriveLow);
        assert_eq!(error.pin_state, GpioPulsePinState::Indeterminate);
        assert_eq!(error.low_errors.len(), 1);
    }
}
