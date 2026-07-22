use std::sync::Arc;

use tauri::State;

use crate::commands::client::with_connection;
use crate::commands::path::DevicePath;
use crate::error::Result;
use crate::flipper::app;
use crate::state::AppState;

/// Launch a Flipper application by name with optional CLI-style args.
/// App-level RPC errors (busy, locked, unknown app) surface as
/// `FlipperError::Rpc` without tearing down the connection — those failures
/// are recoverable from the user's side.
#[tauri::command]
pub async fn app_start(name: String, args: String, state: State<'_, AppState>) -> Result<()> {
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        app::app_start(client, &name, &args)
    })
    .await
}

/// Exit the currently running Flipper application.
#[tauri::command]
pub async fn app_exit(state: State<'_, AppState>) -> Result<()> {
    with_connection(Arc::clone(&state.connection_owner), app::app_exit).await
}

/// Begin Sub-GHz replay of a .sub file via RPC.
///
/// Stock firmware's subghz_app treats a non-empty `args` string as the .sub
/// path to preload — the app lands on the Transmitter scene with the key
/// loaded (but not yet transmitting). We then fire `AppButtonPressRequest`
/// to simulate an OK press, which kicks off the actual radio TX. The app
/// keeps transmitting until [`subghz_tx_stop`] releases the button and exits.
#[tauri::command]
pub async fn subghz_tx_start(path: DevicePath, state: State<'_, AppState>) -> Result<()> {
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        // Launch the Sub-GHz app with the .sub path as args — the app
        // preloads the key and jumps to the Transmitter scene, which is
        // where RPC button events are actually wired up in firmware.
        app::app_start(client, "Sub-GHz", &path)?;
        // Give the scene transition a beat to settle before pressing — the
        // app's RPC handler only registers after it reaches the scene.
        std::thread::sleep(std::time::Duration::from_millis(250));
        // Press OK to start transmission. Best-effort cleanup on failure.
        if let Err(e) = app::app_button_press(client, "") {
            let _ = app::app_button_release(client);
            let _ = app::app_exit(client);
            return Err(e);
        }
        Ok(())
    })
    .await
}

/// Stop an in-progress Sub-GHz replay and exit the app.
#[tauri::command]
pub async fn subghz_tx_stop(state: State<'_, AppState>) -> Result<()> {
    with_connection(Arc::clone(&state.connection_owner), |client| {
        // Release first, then exit. Ignore release errors — the button may not
        // be currently held (e.g. user already pressed Back on the device).
        let _ = app::app_button_release(client);
        app::app_exit(client)
    })
    .await
}
