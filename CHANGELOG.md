# Changelog

All notable changes to FlipperUI are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres (roughly) to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) while pre-1.0.

## [Unreleased]

### Added
- GPIO view (new side-rail entry below the libraries). Shows a vertical illustration of the full 18-pin Flipper Zero header — click any pin to open a detail card on the right. The 8 RPC-controllable pins (PC0, PC1, PC3, PB2, PB3, PA4, PA6, PA7) expose Mode (INPUT/OUTPUT), Pull (none/up/down), live Read with a sparkline of the last 50 samples, Watch-mode polling, and a Write toggle with a quick "Pulse" action. Pin 1 has a +5V OTG switch with a 0.5 A warning. Non-controllable pins (power, ground, SWD debug, I2C bus, 1-Wire) render as info-only cards. Top bar has an OTG mirror chip, a Refresh action, a "Reset all to INPUT (no pull)" safety action, and a poll-interval slider that's floored to 500 ms over BLE. New Rust module + 8 Tauri commands (`gpio_snapshot`, `gpio_set_mode`, `gpio_get_mode`, `gpio_set_pull`, `gpio_read_pin`, `gpio_write_pin`, `gpio_get_otg`, `gpio_set_otg`) wrapping the firmware's existing GPIO RPC messages. The view title carries a small orange "BETA" tag while the feature is stabilizing.
- Right-click context menus on every library view (Sub-GHz, Infrared, NFC, RFID, BadUSB, Apps). Items are tailored per library: SubGhz gets Star/Unstar + Open in Maps (when GPS coords exist) + Rename + Duplicate + Delete; NFC/RFID get Download + Rename + Duplicate + Delete; BadUSB gets Edit + Download + Rename + Duplicate + Delete; Apps gets Launch + Download + Rename + Delete; Infrared gets Rename + Duplicate + Delete. Inline hover-action buttons still work on every row. New shared `ui/ContextMenu.tsx` component; FileBrowser was refactored onto it too so all popups in the app share one implementation.
- Pre-scan heavy-directory review for Sub-GHz / Infrared / NFC / RFID / BadUSB scans. Before each scan the app walks the library roots and lists every directory with 254+ direct entries or files larger than 1 MiB; the user picks which dirs to exclude with checkboxes, and the chosen paths are appended to that library's persistent exclusion list before the real scan starts. New shared Rust prewalk module + `library_prewalk` Tauri command emitting `library-prewalk-progress` events.
- Settings → Library Exclusions → "Pre-scan review" toggle (on by default) to enable/disable the pre-scan review modal across all five libraries. The Apps library is not affected.

### Changed
- Splash window cleaned up: removed the "Starting up…" status line so only the spinner and the FlipperUI wordmark remain, centered in the splash modal.

### Fixed
- BLE File Explorer uploads now use reliable acknowledged writes, smaller BLE storage chunks, byte-based progress, matching ACK handling, and clean reconnect recovery instead of timing out after the file data is queued.
- COM port dropdown items now render with a dark background on Linux/GTK (matched macOS); the native `<option>` elements were inheriting the OS default light background even though the `<select>` itself was themed dark.

## [0.4.0] — 2026-05-11

### Added
- App-icon chooser in Settings → Appearance: pick between the default orange and a dark variant. Live runtime swap of Tauri window icons (Windows taskbar / Linux title bar) and the macOS Dock via `NSApplication.setApplicationIconImage_`. The chosen variant is also written to the running `.app` bundle as a Finder custom icon (`NSWorkspace.setIcon:forFile:`), so the Dock launcher and Finder show it from launch and after quit — no orange-then-dark flash on macOS. The saved variant is re-applied on every launch, so the bundle icon recovers automatically after app updates that reset bundle metadata.
- Forward-proof app-icon variant registry on the Rust side (`app_icon.rs`) so adding new icon variants in the future is one entry plus a PNG.
- Cargo `gen_app_icons` example (gated by the `icon-gen` feature) that recolours warm-orange pixels to near-black to produce the dark icon set across all bundled sizes; rerun after editing source PNGs.
- Unified "Library Exclusions" Settings section: one editor where each row picks a library (Sub-GHz, Infrared, NFC, RFID, BadUSB, Apps) from a dropdown plus the path to exclude. Replaces the six per-library exclusion editors. Underlying storage shape unchanged — Rust scanners still receive their per-library `excludedDirs` arrays.
- Recursive folder download from the File Explorer right-click menu, with cumulative byte-based progress across the whole tree and mid-transfer cancellation.
- File Browser Settings section: per-action toggles (Rename, Download, Delete) to choose which inline hover icons appear on file rows. All actions remain available via right-click context menu regardless of the setting.
- Added a GitHub bug-report issue template with required reproduction context and environment fields.
- Added a GitHub feature-request issue template with workflow, area, and transport-scoped prompts.
- Added issue-template config enabling blank issues.
- GitHub Sponsors metadata via `.github/FUNDING.yml` with Buy Me a Coffee support.
- Screen Stream settings for default screenshot and GIF recording save folders.
- Screen Viewer fullscreen mode with fullscreen sizing, keyboard toggle, and exit shortcuts.
- Automatic Flipper clock synchronization after successful USB or BLE connection, with a Settings toggle.
- BadUSB / BadKB DuckyScript editor backed by CodeMirror, including syntax highlighting, completions, snippets, dirty-state tracking, save/revert controls, and `Cmd/Ctrl+S`.
- Incremental BadUSB path parsing command so edited scripts can refresh metadata without a full library rescan.
- Centralized `with_client` helper with sharper transient-vs-fatal transport-error categorization so recoverable `Interrupted` / `WouldBlock` reads no longer tear down the connection.
- Centralized `validate_path` helper with unit tests covering `/ext`, `/int`, `/any`, lookalike-root rejection, and component-level `..` traversal blocking.
- 1 MiB maximum incoming frame size in the protocol framer to guard against OOM allocations on a corrupt length prefix.

### Changed
- Settings reorganised: the six per-library Sub-GHz / Infrared / NFC / RFID / BadUSB / Apps sections (which only held an excluded-directories editor) collapse into the new "Library Exclusions" section. The Apps section now contains only its "Additional app directories" editor.
- `cli_start`, `cli_send`, `screen_stream_start`, `screen_stream_stop`, and `send_input_event` are now `async` + `spawn_blocking`, so blocking serial I/O no longer freezes the Tauri main thread.
- README updates now include an unsigned-build disclaimer and a macOS quarantine-removal troubleshooting command.
- README clone URL example now uses `https://github.com/fuckmaz/FlipperUI.git`.
- README now includes a maintainer sign-off line and a Stargazers-over-time chart section.
- README star history section now uses the RepoHistory timeline chart.
- Backfilled and expanded `CHANGELOG.md` release history for versions `0.3.0` through `0.3.5`, including updated compare/release links.
- Screen Viewer zoom levels now use cleaner discrete `1x` through `5x` steps.
- BLE connection dialog now shows scan progress as status text instead of exposing a manual stop-scan button.
- BadUSB row action and double-click behavior now open the editor instead of a read-only preview.
- Simplified the leap-year calculation in the device clock-sync path using the standard `is_multiple_of` method.
- Repo-wide `cargo fmt` pass.

### Fixed
- Dashboard battery voltage display now converts millivolts to volts in `BatteryCard` to avoid incorrectly scaled voltage values.
- Device Info battery voltage now also formats from millivolts to volts.
- Screen Viewer fullscreen sizing now preserves the Flipper display's 2:1 aspect ratio, growing the app window when possible and scaling down proportionally on short windows instead of squashing the stream vertically.
- BadUSB edit saves now refresh table metadata such as line count, leading comment, size, and modified time.
- BadUSB rename, duplicate, and delete actions now operate against the full library list instead of accidentally persisting only the currently filtered rows.
- Path-validation `/ext` prefix bug — paths like `/extABC` are now correctly rejected (previously a `starts_with("/ext")` slip let lookalike roots through).
- `send_input_event` no longer holds a mutex across the channel send into the screen-reader thread, removing a contention/interleaving hazard during rapid input bursts.
- Tauri `listen()` promise race fixed across ~10 frontend sites (App.tsx, all six library views, BleDialog, FileBrowser) — listeners registered after a component unmount now self-cancel instead of leaking.
- Removed a duplicate BLE reconnect handler in `DevicePanel.tsx` that was racing the canonical handler in `App.tsx`.

## [0.3.5] — 2026-05-05

### Added
- OS notifications for completed library scans, transfers, and unexpected device disconnects, with a Settings toggle.
- Tauri notification plugin wiring across frontend and backend.
- More polished first-release README content and GitHub-facing project presentation.
- Release screenshot asset for the dashboard preview.

### Changed
- Refactored frontend/backend code structure for readability and maintainability.
- Polished the Screen Stream and GIF recorder UI.
- Replaced text-heavy Connect/Disconnect controls with compact icon controls.
- Standardized CI artifact names for macOS, Windows, and Linux bundles.
- Gated the native app menu to macOS so Windows does not render an unwanted gray menu strip below the title bar.

### Fixed
- Improved session-management formatting and error handling.
- Windows menu-bar appearance now follows the in-app dark UI more cleanly.

## [0.3.4] — 2026-04-30

### Added
- Global Search for indexed library entries and the currently loaded File Explorer directory.
- Search result routing into Apps, Sub-GHz, Infrared, NFC, RFID, BadUSB, and Files.
- Windows CI bundle builds and uploaded build artifacts for macOS, Windows, and Linux.

### Changed
- Global Search now expands/collapses from the header, includes a close control, and keeps the header layout tighter.
- Increased the default main-window height to improve dense dashboard and library layouts.

### Fixed
- Windows USB serial connection handling.
- USB auto-connect loop on Windows by adding a cooldown after failed automatic connection attempts.
- Error banner placement so connection and runtime errors are visible in the main layout.

## [0.3.3] — 2026-04-30

### Added
- RFID / 125 kHz library with recursive scanning, metadata parsing, filtering, caching, upload, download, rename, and delete flows.
- Sub-GHz favorites with star toggles and a starred-only filter.
- Modified-time columns across library tables.
- Sorting by modification time for library rows.
- Shared formatting and path helpers for library views.

### Changed
- DevicePanel now shows a battery percentage chip with a progress bar.
- Dashboard links to the detailed Device Info view directly.
- Device Info was removed from the primary side rail to keep navigation focused.
- Library upload flows no longer trigger an unnecessary full refresh after upload.

### Fixed
- BatteryChip display for firmware variants that report battery fields under different key names.
- TypeScript and cross-platform path issues affecting macOS and Windows builds.
- BLE auto-reconnect behavior after connection breaks.

## [0.3.2] — 2026-04-28

### Added
- Visual bezel/backlight border around the Screen Stream canvas to better match the physical Flipper display.
- DevicePanel battery chip with live percentage and charging state.
- Dashboard quick link into the detailed Device Info page.

### Changed
- Adjusted default app height for a better first-run layout.
- Removed an unused dashboard spinner asset.

### Fixed
- BLE screen-view reliability, including control-command queuing during active streaming.
- Screen input handling during rapid controls and long-press style interactions.

## [0.3.1] — 2026-04-27

### Added
- BLE auto-reconnect after unexpected connection breaks or failures.
- Persisted connection type so the app remembers USB vs BLE between launches.
- On-device Settings UI backed by files under `/int`.
- File Explorer internal/external storage toggle.

### Changed
- Redesigned the DevicePanel info bar for clearer connected-device status.
- BLE scan and connection logic received another round of reliability improvements.

### Fixed
- BLE screen-stream framing and reader issues.
- Race conditions between screen streaming and control commands.
- Storage command teardown behavior while screen streaming is active.

## [0.3.0] — 2026-04-24

### Added
- Dashboard view with device overview, firmware, battery, storage, and library status.
- Command Palette for quick navigation and device actions.
- Drag-in uploads in the File Explorer, including folder hover targeting and a drop overlay.
- Drag-to-Finder export hooks for library rows.
- System tray / menubar icon with Show/Hide/Quit controls.
- macOS Dock visibility setting for menubar-style usage.
- Splashscreen window flow and delayed main-window reveal.
- ESLint configuration and initial CI workflow.
- First structured `CHANGELOG.md`.

### Changed
- README expanded with feature list, quick start, architecture notes, and troubleshooting.
- Settings gained tray, dock, and scan-path controls.
- File Browser upload/download behavior was tightened around progress and cancellation.

### Fixed
- BLE `read_exact` is now atomic with no partial consumption on timeout, fixing intermittent protobuf decode failures when messages span notifications.
- CLI startup now stops an active screen stream before entering CLI mode.
- Several storage and RPC paths now handle active screen streaming more safely.

## [0.2.3] — 2026-04-23

### Added
- iOS-style USB/BLE transport toggle in the Device panel. USB auto-connect is suppressed while BLE is selected.
- Shared `ScanProgressBar` component with an indeterminate animation for the pre-first-event dead zone.
- Live `<datalist>` autocompletion on Settings paths (excluded dirs, extra roots) via the new `useDirectorySuggestions` hook.

### Changed
- BLE scan duration bumped from 1.8 s to 10 s so slower-advertising devices show up reliably.
- Library-scan command boilerplate collapsed into a shared `run_library_scan` helper; subghz / ir / nfc / badusb command modules reduced to ~25 lines each.

## [0.2.2] — 2026-04-22

### Added
- BadUSB library: scanning, filtering, preview modal, per-device cache.
- `FilePreviewModal` for inspecting script contents before write.
- `LibraryTable`, `LibraryToolbar`, and BadUSB-specific icons.

## [0.2.0] — 2026-04-21

### Added
- BLE transport — file browser and library operations work over Bluetooth LE. CLI-over-BLE is not supported.

## [0.1.2]

### Added
- SubGHz, Infrared, and NFC libraries with metadata parsing and per-device caching.
- Dedicated System Info pane (firmware, hardware UID, battery, storage).
- Screen Stream and CLI promoted to top-level nav items.

### Fixed
- Stability and performance improvements across the board.

## [0.1.1]

### Added
- Screen streaming with keyboard controls.
- CLI implementation over serial.
- Screen recorder + GIF export (60 s cap, 2-color palette).
- Developer diagnostics panel — 500-entry ring buffer of TX/RX frames with time, direction, command id, kind, byte count, and status.
- Settings pane with version, credit, and entry point to Developer diagnostics.
- Custom macOS menu bar (FlipperUI / Edit / Window submenus; Cmd+, → Settings).
- New 1024×1024 app icon generated via `tauri icon`.
- Bundle copyright "in love -maz" on the About dialog.

### Fixed
- Screen-stream freeze when holding a button — input events now route through an mpsc channel consumed by the reader thread, serializing writes and reads on one thread and eliminating the mutex starvation that corrupted frame framing.
- Stale "connected" UI after an unrecoverable reader error — Rust tears down the client and emits `flipper-disconnected`; `App.tsx` flips the UI accordingly.

## [0.1.0] — Initial commits

- Dark theme.
- Basic file browser and device info.
- Initial Tauri v2 scaffolding.

[Unreleased]: https://github.com/fuckmaz/FlipperUI/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/fuckmaz/FlipperUI/releases/tag/v0.4.0
[0.3.5]: https://github.com/fuckmaz/FlipperUI/releases/tag/v0.3.5
[0.3.4]: https://github.com/fuckmaz/FlipperUI/commit/93b85cc
[0.3.3]: https://github.com/fuckmaz/FlipperUI/commit/fc62325
[0.3.2]: https://github.com/fuckmaz/FlipperUI/commit/7116d52
[0.3.1]: https://github.com/fuckmaz/FlipperUI/commit/5114ac9
[0.3.0]: https://github.com/fuckmaz/FlipperUI/commit/bf4b916
[0.2.3]: https://github.com/fuckmaz/FlipperUI/commit/d319662
[0.2.2]: https://github.com/fuckmaz/FlipperUI/commit/f06153f
[0.2.0]: https://github.com/fuckmaz/FlipperUI/commit/0eadc2e
[0.1.2]: https://github.com/fuckmaz/FlipperUI/commit/044abd8
[0.1.1]: https://github.com/fuckmaz/FlipperUI/commit/8cc675a
[0.1.0]: https://github.com/fuckmaz/FlipperUI/commit/88af77c
