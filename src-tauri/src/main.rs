// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    #[cfg(target_os = "linux")]
    apply_linux_webkit_env_defaults();
    flipperui_lib::run()
}

/// WebKitGTK settings that keep the window from coming up blank on common
/// Linux GPU setups (issue #8):
/// - the DMA-BUF renderer needs a working GBM/EGL display, which fails on
///   NVIDIA ("Could not create GBM EGL display") and some Mesa setups;
/// - NVIDIA's explicit sync breaks WebKit on Wayland
///   (<https://v2.tauri.app/develop/debug/linux-graphics/>).
#[cfg(any(target_os = "linux", test))]
fn linux_webkit_env_defaults(nvidia: bool) -> Vec<(&'static str, &'static str)> {
    let mut defaults = vec![("WEBKIT_DISABLE_DMABUF_RENDERER", "1")];
    if nvidia {
        defaults.push(("__NV_DISABLE_EXPLICIT_SYNC", "1"));
    }
    defaults
}

/// Apply [`linux_webkit_env_defaults`] without overriding anything the user
/// set themselves. Runs first thing in `main`, before any other thread exists
/// or GTK reads its environment.
#[cfg(target_os = "linux")]
fn apply_linux_webkit_env_defaults() {
    let nvidia = std::path::Path::new("/proc/driver/nvidia/version").exists();
    for (key, value) in linux_webkit_env_defaults(nvidia) {
        if std::env::var_os(key).is_none() {
            std::env::set_var(key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::linux_webkit_env_defaults;

    #[test]
    fn dmabuf_renderer_is_always_disabled() {
        assert_eq!(
            linux_webkit_env_defaults(false),
            vec![("WEBKIT_DISABLE_DMABUF_RENDERER", "1")]
        );
    }

    #[test]
    fn nvidia_also_disables_explicit_sync() {
        assert!(linux_webkit_env_defaults(true).contains(&("__NV_DISABLE_EXPLICIT_SYNC", "1")));
    }
}
