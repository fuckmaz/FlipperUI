use serde::{Deserialize, Serialize};

use crate::error::{FlipperError, Result};
use crate::flipper::client::FlipperClient;
use crate::flipper::framing::{read_response, write_message};
use crate::flipper::session::check_response;
use crate::pb;
use crate::pb::main::Content;
use crate::pb_gpio;

/// Single-pin state snapshot returned by [`snapshot`]. Mirrors the structure
/// the frontend consumes from the `gpio_snapshot` command — string-typed pin
/// and mode (the proto enum names — `"PC0"`, `"input"`/`"output"`) plus the
/// last-read value (0 or 1).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpioPinSnapshot {
    pub pin: String,
    pub mode: String,
    pub value: u8,
}

/// Full GPIO state snapshot: every pin's mode/value plus the current OTG flag.
/// Built by issuing N+1 RPC round-trips (one `get_pin_mode` + one `read_pin`
/// per pin, plus a final `get_otg_mode`). Sequential is fine — the device
/// handles GPIO RPC calls quickly and we don't need to over-engineer it.
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
        Some(Content::GpioGetPinModeResponse(r)) => pb_gpio::GpioPinMode::try_from(r.mode)
            .map_err(|_| FlipperError::UnexpectedResponse),
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
        content: Some(Content::GpioReadPin(pb_gpio::ReadPin {
            pin: pin as i32,
        })),
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
pub fn write_pin(
    client: &mut FlipperClient,
    pin: pb_gpio::GpioPin,
    value: u32,
) -> Result<()> {
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
        Some(Content::GpioGetOtgModeResponse(r)) => pb_gpio::GpioOtgMode::try_from(r.mode)
            .map_err(|_| FlipperError::UnexpectedResponse),
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

/// Convenience that walks every pin and emits a [`GpioSnapshot`]. Issues one
/// `get_pin_mode` + `read_pin` per pin plus one `get_otg_mode` — 17 RPC calls
/// total for the standard 8-pin header. Sequential by design: the firmware
/// serialises RPC requests anyway and the call is cheap.
pub fn snapshot(client: &mut FlipperClient) -> Result<GpioSnapshot> {
    let mut pins = Vec::with_capacity(ALL_PINS.len());
    for pin in ALL_PINS {
        let mode = get_pin_mode(client, pin)?;
        let value = read_pin(client, pin)?;
        pins.push(GpioPinSnapshot {
            pin: pin.as_str_name().to_string(),
            mode: match mode {
                pb_gpio::GpioPinMode::Input => "input".to_string(),
                pb_gpio::GpioPinMode::Output => "output".to_string(),
            },
            // The firmware returns the raw GPIO sample as a uint32, but the
            // pin is digital so anything non-zero is "high". Clamp to 0/1 so
            // the JS side gets a predictable boolean-ish u8.
            value: if value == 0 { 0 } else { 1 },
        });
    }
    let otg = matches!(get_otg_mode(client)?, pb_gpio::GpioOtgMode::On);
    Ok(GpioSnapshot { pins, otg })
}
