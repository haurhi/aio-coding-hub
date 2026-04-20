//! Usage: Gateway startup and follow-up sync for bootstrap.

use super::app_state::GatewayState;
use crate::shared::mutex_ext::MutexExt;
use crate::{blocking, cli_proxy};
use tauri::Manager;

pub(crate) async fn start(
    app_handle: &tauri::AppHandle,
    db: crate::db::Db,
    settings: &crate::settings::AppSettings,
) -> Option<crate::gateway::GatewayStatus> {
    let preferred_port = settings.preferred_port;
    let enable_cli_proxy_startup_recovery = settings.enable_cli_proxy_startup_recovery;

    let status = match blocking::run("startup_gateway_autostart", {
        let app_handle = app_handle.clone();
        let db = db.clone();
        move || {
            let state = app_handle.state::<GatewayState>();
            let mut manager = state.0.lock_or_recover();
            manager.start(&app_handle, db, Some(preferred_port))
        }
    })
    .await
    {
        Ok(status) => status,
        Err(err) => {
            tracing::error!("gateway auto-start failed: {}", err);
            if enable_cli_proxy_startup_recovery {
                crate::app::cleanup::restore_cli_proxy_keep_state_best_effort(
                    app_handle,
                    "startup_cli_proxy_restore_keep_state",
                    "startup_recovery_gateway_failed",
                    true,
                )
                .await;
            }
            return None;
        }
    };

    crate::app::heartbeat_watchdog::gated_emit(
        app_handle,
        crate::gateway::events::GATEWAY_STATUS_EVENT_NAME,
        status.clone(),
    );

    Some(status)
}

pub(crate) async fn sync_cli_proxy_after_autostart(
    app_handle: &tauri::AppHandle,
    status: &crate::gateway::GatewayStatus,
) {
    if let Some(base_origin) = status.base_url.as_deref() {
        let app_for_sync = app_handle.clone();
        let base_origin = base_origin.to_string();
        let _ = blocking::run("cli_proxy_sync_enabled_after_autostart", move || {
            cli_proxy::sync_enabled(&app_for_sync, &base_origin, true)
        })
        .await;
    }
}
