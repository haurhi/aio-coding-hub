//! Usage: Async startup task pipeline extracted from bootstrap setup.

use super::app_state::{ensure_db_ready, DbInitState};
use tauri::Manager;

pub(crate) fn spawn(app_handle: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        run(app_handle).await;
    });
}

async fn run(app_handle: tauri::AppHandle) {
    let db_state = app_handle.state::<DbInitState>();
    let db = match ensure_db_ready(app_handle.clone(), db_state.inner()).await {
        Ok(db) => db,
        Err(err) => {
            tracing::error!("database initialization failed: {}", err);
            return;
        }
    };

    let settings = match crate::app::startup_settings::read(&app_handle).await {
        Ok(settings) => settings,
        Err(()) => return,
    };

    crate::app::startup_settings::apply_window_state(&app_handle, &settings);

    let status = match crate::app::startup_gateway::start(&app_handle, db.clone(), &settings).await
    {
        Some(status) => status,
        None => return,
    };

    crate::app::startup_gateway::sync_cli_proxy_after_autostart(&app_handle, &status).await;
    crate::app::startup_wsl::finalize(&app_handle, db, status.port, settings).await;
}
