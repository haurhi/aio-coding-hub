//! Usage: Application layer (Tauri-managed state, tray/window lifecycle, startup wiring).

pub(crate) mod app_state;
pub(crate) mod autostart;
pub(crate) mod bootstrap;
pub(crate) mod cleanup;
pub(crate) mod heartbeat_watchdog;
pub(crate) mod lifecycle;
pub(crate) mod linux_webkit_compat;
pub(crate) mod logging;
pub(crate) mod notice;
pub(crate) mod plugin_registry;
pub(crate) mod resident;
pub(crate) mod startup_gateway;
pub(crate) mod startup_settings;
pub(crate) mod startup_tasks;
pub(crate) mod startup_wsl;
