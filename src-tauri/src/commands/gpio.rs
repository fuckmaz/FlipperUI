use std::sync::Arc;

use tauri::State;

use crate::commands::client::with_client;
use crate::error::{FlipperError, Result};
use crate::flipper::gpio;
use crate::pb_gpio;
use crate::state::AppState;

// Re-export the snapshot types so the frontend's generated bindings (and
// `tauri::generate_handler!`) can find them through the commands module.
pub use crate::flipper::gpio::{GpioPinSnapshot, GpioSnapshot};

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

/// Snapshot every GPIO pin's mode + value plus the OTG flag. Issues N+1 RPC
/// calls sequentially under a single `with_client` so other commands don't
/// observe the device mid-walk.
#[tauri::command]
pub async fn gpio_snapshot(state: State<'_, AppState>) -> Result<GpioSnapshot> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);

    tauri::async_runtime::spawn_blocking(move || {
        with_client(&mode_mutex, &client_mutex, gpio::snapshot)
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

#[tauri::command]
pub async fn gpio_set_mode(pin: String, mode: String, state: State<'_, AppState>) -> Result<()> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);

    tauri::async_runtime::spawn_blocking(move || {
        let pin = parse_pin(&pin)?;
        let mode = parse_mode(&mode)?;
        with_client(&mode_mutex, &client_mutex, |c| {
            gpio::set_pin_mode(c, pin, mode)
        })
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

#[tauri::command]
pub async fn gpio_get_mode(pin: String, state: State<'_, AppState>) -> Result<String> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);

    tauri::async_runtime::spawn_blocking(move || {
        let pin = parse_pin(&pin)?;
        let mode = with_client(&mode_mutex, &client_mutex, |c| gpio::get_pin_mode(c, pin))?;
        Ok(match mode {
            pb_gpio::GpioPinMode::Input => "input".to_string(),
            pb_gpio::GpioPinMode::Output => "output".to_string(),
        })
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

#[tauri::command]
pub async fn gpio_set_pull(pin: String, pull: String, state: State<'_, AppState>) -> Result<()> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);

    tauri::async_runtime::spawn_blocking(move || {
        let pin = parse_pin(&pin)?;
        let pull = parse_pull(&pull)?;
        with_client(&mode_mutex, &client_mutex, |c| {
            gpio::set_input_pull(c, pin, pull)
        })
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

#[tauri::command]
pub async fn gpio_read_pin(pin: String, state: State<'_, AppState>) -> Result<u8> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);

    tauri::async_runtime::spawn_blocking(move || {
        let pin = parse_pin(&pin)?;
        let value = with_client(&mode_mutex, &client_mutex, |c| gpio::read_pin(c, pin))?;
        // The firmware returns a raw uint32 sample — clamp to 0/1 so the JS
        // side sees a tidy boolean-ish u8 matching the snapshot encoding.
        Ok(if value == 0 { 0u8 } else { 1u8 })
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

#[tauri::command]
pub async fn gpio_write_pin(pin: String, value: u8, state: State<'_, AppState>) -> Result<()> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);

    tauri::async_runtime::spawn_blocking(move || {
        let pin = parse_pin(&pin)?;
        let value = u32::from(value);
        with_client(&mode_mutex, &client_mutex, |c| {
            gpio::write_pin(c, pin, value)
        })
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

#[tauri::command]
pub async fn gpio_get_otg(state: State<'_, AppState>) -> Result<bool> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);

    tauri::async_runtime::spawn_blocking(move || {
        let mode = with_client(&mode_mutex, &client_mutex, gpio::get_otg_mode)?;
        Ok(matches!(mode, pb_gpio::GpioOtgMode::On))
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

#[tauri::command]
pub async fn gpio_set_otg(on: bool, state: State<'_, AppState>) -> Result<()> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);

    tauri::async_runtime::spawn_blocking(move || {
        let mode = if on {
            pb_gpio::GpioOtgMode::On
        } else {
            pb_gpio::GpioOtgMode::Off
        };
        with_client(&mode_mutex, &client_mutex, |c| gpio::set_otg_mode(c, mode))
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
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
}
