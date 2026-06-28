pub mod app_icon;
pub mod commands;
pub mod error;
pub mod flipper;
pub mod state;

// Include prost-generated protobuf bindings.
// pb.rs references all other packages, so they must all be declared here.
pub mod pb_app {
    include!(concat!(env!("OUT_DIR"), "/pb_app.rs"));
}
pub mod pb_desktop {
    include!(concat!(env!("OUT_DIR"), "/pb_desktop.rs"));
}
pub mod pb_gpio {
    include!(concat!(env!("OUT_DIR"), "/pb_gpio.rs"));
}
pub mod pb_gui {
    include!(concat!(env!("OUT_DIR"), "/pb_gui.rs"));
}
pub mod pb_property {
    include!(concat!(env!("OUT_DIR"), "/pb_property.rs"));
}
pub mod pb_storage {
    include!(concat!(env!("OUT_DIR"), "/pb_storage.rs"));
}
pub mod pb_system {
    include!(concat!(env!("OUT_DIR"), "/pb_system.rs"));
}
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/pb.rs"));
}

use state::AppState;
#[cfg(target_os = "macos")]
use tauri::menu::{AboutMetadataBuilder, PredefinedMenuItem, SubmenuBuilder};
use tauri::menu::{CheckMenuItemBuilder, Menu, MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Render a 10-segment percentage bar using filled/empty Unicode block glyphs.
/// Used in the tray flyout battery row so users get a quick visual read on
/// charge level without parsing the digits.
fn battery_bar(charge: u8) -> String {
    const WIDTH: u8 = 10;
    let pct = charge.min(100) as u16;
    let filled = (pct * WIDTH as u16 / 100) as u8;
    let mut bar = String::with_capacity(WIDTH as usize * 3);
    for _ in 0..filled {
        bar.push('▰');
    }
    for _ in 0..WIDTH.saturating_sub(filled) {
        bar.push('▱');
    }
    bar
}

/// Build the tray flyout menu: device status header (when connected),
/// navigation shortcuts to each major view, then a window-visible toggle and
/// Quit. The menu is rebuilt on every status push so the battery bar and
/// device fields stay live.
pub fn build_tray_menu(
    app: &tauri::AppHandle,
    status: &commands::tray::TrayStatus,
) -> tauri::Result<Menu<tauri::Wry>> {
    let mut builder = MenuBuilder::new(app);

    // Status header — disabled items used purely as informational labels.
    if status.connected {
        let title = match (&status.device_name, &status.firmware_version) {
            (Some(n), Some(v)) => format!("{n}  ·  {v}"),
            (Some(n), None) => n.clone(),
            (None, Some(v)) => v.clone(),
            _ => "Flipper Zero".to_string(),
        };
        let header = MenuItemBuilder::with_id("tray-status-device", title)
            .enabled(false)
            .build(app)?;
        builder = builder.item(&header);

        if let Some(charge) = status.battery_charge {
            let bolt = if status.battery_charging { "  ⚡" } else { "" };
            let bar = battery_bar(charge);
            let battery = MenuItemBuilder::with_id(
                "tray-status-battery",
                format!("Battery  {bar}  {charge}%{bolt}"),
            )
            .enabled(false)
            .build(app)?;
            builder = builder.item(&battery);
        }
    } else {
        let header = MenuItemBuilder::with_id("tray-status-device", "○ Disconnected")
            .enabled(false)
            .build(app)?;
        builder = builder.item(&header);
    }
    builder = builder.separator();

    // Window visibility toggle — replaces the previous Show/Hide pair with a
    // single checkable item so the current state is visible at a glance.
    let window_visible = app
        .get_webview_window("main")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false);
    let visible_toggle = CheckMenuItemBuilder::with_id("tray-window-toggle", "Window Visible")
        .checked(window_visible)
        .build(app)?;
    builder = builder.item(&visible_toggle).separator();

    // Navigation shortcuts. The frontend listens for "tray-nav" events and
    // calls setActiveView with the payload. Items that require a connection
    // (libraries) are disabled when no device is attached.
    let nav_dashboard =
        MenuItemBuilder::with_id("tray-nav-dashboard", "Open Dashboard").build(app)?;
    let nav_files = MenuItemBuilder::with_id("tray-nav-files", "File Explorer")
        .enabled(status.connected)
        .build(app)?;
    let nav_subghz = MenuItemBuilder::with_id("tray-nav-subghz", "Sub-GHz Library")
        .enabled(status.connected)
        .build(app)?;
    let nav_infrared = MenuItemBuilder::with_id("tray-nav-infrared", "Infrared Library")
        .enabled(status.connected)
        .build(app)?;
    let nav_nfc = MenuItemBuilder::with_id("tray-nav-nfc", "NFC Library")
        .enabled(status.connected)
        .build(app)?;
    let nav_rfid = MenuItemBuilder::with_id("tray-nav-rfid", "RFID Library")
        .enabled(status.connected)
        .build(app)?;
    let nav_badusb = MenuItemBuilder::with_id("tray-nav-badusb", "BadUSB Library")
        .enabled(status.connected)
        .build(app)?;
    let nav_apps = MenuItemBuilder::with_id("tray-nav-apps", "App Library")
        .enabled(status.connected)
        .build(app)?;
    let nav_settings = MenuItemBuilder::with_id("tray-nav-settings", "Settings…").build(app)?;
    builder = builder
        .item(&nav_dashboard)
        .item(&nav_files)
        .item(&nav_subghz)
        .item(&nav_infrared)
        .item(&nav_nfc)
        .item(&nav_rfid)
        .item(&nav_badusb)
        .item(&nav_apps)
        .separator()
        .item(&nav_settings)
        .separator();

    let quit = MenuItemBuilder::with_id("tray-quit", "Quit FlipperUI").build(app)?;
    builder.item(&quit).build()
}

/// Rebuild the tray menu with the latest cached status. Called whenever
/// something that affects the menu changes outside of `update_tray_status`,
/// e.g. the window visibility toggle.
pub fn refresh_tray_menu(app: &tauri::AppHandle) -> tauri::Result<()> {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let menu = build_tray_menu(app, &commands::tray::tray_status())?;
        tray.set_menu(Some(menu))?;
    }
    Ok(())
}

pub const TRAY_ID: &str = "main-tray";

/// Raw PNG bytes for the monochrome tray glyph. Pre-sized to fit menubar
/// proportions; on macOS we mark it as a template image so it adopts the
/// menubar's foreground color automatically.
const TRAY_MONOCHROME_PNG: &[u8] = include_bytes!("../icons/tray-monochrome.png");

/// Resolve the icon to use for the tray. `monochrome` swaps in the flat glyph;
/// otherwise we use the app's default window icon. Exposed under
/// `tray_icon_for` so commands::tray can re-skin a running tray without
/// rebuilding it.
pub fn tray_icon_for(
    _app: &tauri::AppHandle,
    monochrome: bool,
) -> tauri::Result<tauri::image::Image<'static>> {
    if monochrome {
        let raw = tauri::image::Image::from_bytes(TRAY_MONOCHROME_PNG)?;
        // macOS scales tray icons to fit the menubar height. To make the
        // glyph render at ~50% of menubar height, we centre it inside a
        // square transparent canvas double the source's max dimension.
        return Ok(pad_to_square(&raw, 1));
    }
    tauri::image::Image::from_bytes(app_icon::current_variant_png())
        .map_err(|e| tauri::Error::AssetNotFound(format!("app-icon variant PNG: {e}")))
}

/// Centre `src` inside a square transparent RGBA canvas whose side equals
/// `multiplier × max(src.width, src.height)`. Used to add empty padding
/// around tray glyphs so the OS scales the visible mark to a fraction of
/// the menubar height instead of filling it edge-to-edge.
fn pad_to_square(src: &tauri::image::Image<'_>, multiplier: u32) -> tauri::image::Image<'static> {
    let sw = src.width();
    let sh = src.height();
    let side = sw.max(sh).saturating_mul(multiplier).max(1);
    let mut canvas = vec![0u8; (side as usize) * (side as usize) * 4];
    let off_x = ((side - sw) / 2) as usize;
    let off_y = ((side - sh) / 2) as usize;
    let stride = side as usize * 4;
    let row_bytes = sw as usize * 4;
    let src_bytes = src.rgba();
    for row in 0..sh as usize {
        let dst_off = (off_y + row) * stride + off_x * 4;
        let src_off = row * row_bytes;
        canvas[dst_off..dst_off + row_bytes]
            .copy_from_slice(&src_bytes[src_off..src_off + row_bytes]);
    }
    tauri::image::Image::new(&canvas, side, side).to_owned()
}

/// Build and install the system-tray icon. Shared between the initial startup
/// path and the runtime toggle command so the click-handling behaviour stays
/// identical.
pub fn install_tray(app: &tauri::AppHandle, monochrome: bool) -> tauri::Result<()> {
    let tray_menu = build_tray_menu(app, &commands::tray::tray_status())?;
    let tray = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("FlipperUI")
        .icon(tray_icon_for(app, monochrome)?)
        .menu(&tray_menu)
        // Left-click opens the flyout menu (device status + nav shortcuts);
        // right-click also opens it via the platform's native gesture.
        .show_menu_on_left_click(true)
        .on_tray_icon_event(|tray, event| {
            // Suppress the rare middle-click toggle path: the flyout menu is
            // now the primary interaction. We only handle middle-click as a
            // window-toggle convenience.
            if let TrayIconEvent::Click {
                button: MouseButton::Middle,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(w) = app.get_webview_window("main") {
                    match w.is_visible().unwrap_or(false) {
                        true => {
                            let _ = w.hide();
                        }
                        false => {
                            let _ = w.show();
                            let _ = w.set_focus();
                            let _ = w.unminimize();
                        }
                    }
                }
            }
        })
        .build(app)?;
    let _ = tray.set_icon_as_template(monochrome);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Initialize logging
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    tracing::info!("FlipperUI starting up");

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_drag::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_notification::init())
        .manage(AppState::new())
        .setup(|app| {
            // Custom app-menu with a "Settings…" item (Cmd+,). Clicking it
            // emits "open-settings" so the frontend can open the dialog.
            // We also reconstruct the standard Edit + Window submenus so
            // Cut/Copy/Paste and Minimize/Close keep working — replacing the
            // default menu drops those unless we add them back.
            //
            // macOS only: on Windows/Linux a top-level app menu renders as a
            // grey strip below the title bar (the platform "menu bar"), which
            // clashes with our own header. The submenu items here are all
            // either macOS-specific (services/hide/show-all) or already
            // covered by native shortcuts (Cut/Copy/Paste, Minimize/Close)
            // and in-app UI (Settings via tray + sidebar), so we just skip
            // installing the menu on non-mac platforms.
            #[cfg(target_os = "macos")]
            {
                let about_meta = AboutMetadataBuilder::new()
                    .name(Some("FlipperUI"))
                    .version(Some(env!("CARGO_PKG_VERSION")))
                    .copyright(Some("in love -maz"))
                    .build();

                let settings = MenuItemBuilder::with_id("settings", "Settings…")
                    .accelerator("CmdOrCtrl+,")
                    .build(app)?;

                let app_submenu = SubmenuBuilder::new(app, "FlipperUI")
                    .item(&PredefinedMenuItem::about(
                        app,
                        Some("About FlipperUI"),
                        Some(about_meta),
                    )?)
                    .separator()
                    .item(&settings)
                    .separator()
                    .services()
                    .separator()
                    .hide()
                    .hide_others()
                    .show_all()
                    .separator()
                    .quit()
                    .build()?;

                let edit_submenu = SubmenuBuilder::new(app, "Edit")
                    .undo()
                    .redo()
                    .separator()
                    .cut()
                    .copy()
                    .paste()
                    .select_all()
                    .build()?;

                let window_submenu = SubmenuBuilder::new(app, "Window")
                    .minimize()
                    .separator()
                    .close_window()
                    .build()?;

                let menu = MenuBuilder::new(app)
                    .items(&[&app_submenu, &edit_submenu, &window_submenu])
                    .build()?;
                app.set_menu(menu)?;
            }

            app.on_menu_event(|app, event| {
                let id = event.id().as_ref();
                // Tray nav shortcuts: bring the window forward and emit the
                // view name. The frontend listens for "tray-nav" and calls
                // setActiveView.
                if let Some(view) = id.strip_prefix("tray-nav-") {
                    if let Some(w) = app.get_webview_window("main") {
                        let _ = w.show();
                        let _ = w.set_focus();
                        let _ = w.unminimize();
                    }
                    let _ = app.emit("tray-nav", view.to_string());
                    let _ = refresh_tray_menu(app);
                    return;
                }
                match id {
                    "settings" => {
                        let _ = app.emit("open-settings", ());
                    }
                    // Tray menu items — toggle window visibility / quit.
                    "tray-window-toggle" => {
                        if let Some(w) = app.get_webview_window("main") {
                            if w.is_visible().unwrap_or(false) {
                                let _ = w.hide();
                            } else {
                                let _ = w.show();
                                let _ = w.set_focus();
                                let _ = w.unminimize();
                            }
                        }
                        let _ = refresh_tray_menu(app);
                    }
                    "tray-quit" => {
                        app.exit(0);
                    }
                    _ => {}
                }
            });

            // System-tray icon. Left-click toggles the window; right-click
            // (or the context menu gesture) opens the Show/Hide/Quit menu.
            // The frontend can remove and re-install the tray at runtime via
            // `set_tray_enabled`, so the build logic lives in a shared helper.
            // App-icon variant — applied before the tray so install_tray
            // picks up the correct variant icon from the start.
            let variant = app_icon::load_saved_variant(app.handle());
            if let Err(e) = app_icon::apply_app_icon(app.handle(), variant) {
                tracing::warn!("app-icon: initial apply failed: {e}");
            }

            install_tray(app.handle(), commands::tray::tray_monochrome())?;

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::device::list_ports,
            commands::device::connect,
            commands::device::disconnect,
            commands::device::list_ble_devices,
            commands::device::start_ble_scan,
            commands::device::stop_ble_scan,
            commands::device::connect_ble_device,
            commands::device::connection_kind,
            commands::storage::storage_list,
            commands::storage::storage_stat,
            commands::storage::storage_read,
            commands::storage::storage_write,
            commands::storage::storage_read_to_local,
            commands::storage::storage_read_dir_to_local,
            commands::storage::storage_write_from_local,
            commands::storage::storage_mkdir,
            commands::storage::storage_delete,
            commands::storage::storage_rename,
            commands::storage::storage_info,
            commands::storage::storage_du,
            commands::storage::storage_timestamp,
            commands::storage::storage_tar_extract,
            commands::storage::cancel_transfer,
            commands::device::power_info,
            commands::device::device_info_all,
            commands::device::ping,
            commands::device::sync_clock,
            commands::device::reboot,
            commands::firmware::firmware_providers,
            commands::firmware::firmware_fetch_directory,
            commands::firmware::firmware_flash,
            commands::cli::cli_start,
            commands::cli::cli_send,
            commands::cli::cli_stop,
            commands::gui::screen_stream_start,
            commands::gui::screen_stream_stop,
            commands::gui::send_input_event,
            commands::diag::diag_enable,
            commands::diag::diag_entries,
            commands::diag::diag_clear,
            commands::diag::diag_is_enabled,
            commands::app::app_start,
            commands::app::app_exit,
            commands::app::subghz_tx_start,
            commands::app::subghz_tx_stop,
            commands::subghz::subghz_scan,
            commands::subghz::subghz_cancel_scan,
            commands::infrared::infrared_scan,
            commands::infrared::infrared_cancel_scan,
            commands::nfc::nfc_scan,
            commands::nfc::nfc_cancel_scan,
            commands::nfc::nfc_parse_paths,
            commands::rfid::rfid_scan,
            commands::rfid::rfid_cancel_scan,
            commands::rfid::rfid_parse_paths,
            commands::badusb::badusb_scan,
            commands::badusb::badusb_cancel_scan,
            commands::badusb::badusb_parse_paths,
            commands::library_prewalk::library_prewalk,
            commands::apps::apps_scan,
            commands::apps::apps_cancel_scan,
            commands::apps::apps_parse_paths,
            commands::apps::apps_read_icon,
            commands::window::close_splashscreen,
            commands::tray::set_tray_enabled,
            commands::tray::set_dock_visible,
            commands::tray::set_tray_monochrome,
            commands::tray::update_tray_status,
            commands::app_icon::app_icon_variants,
            commands::app_icon::set_app_icon,
            commands::gpio::gpio_snapshot,
            commands::gpio::gpio_set_mode,
            commands::gpio::gpio_get_mode,
            commands::gpio::gpio_set_pull,
            commands::gpio::gpio_read_pin,
            commands::gpio::gpio_write_pin,
            commands::gpio::gpio_get_otg,
            commands::gpio::gpio_set_otg,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
