//! Raw links are inert data. The shared strict parser must accept them before a review UI is shown.
use tauri::AppHandle;
use tauri_plugin_deep_link::DeepLinkExt;

#[tauri::command]
pub fn desktop_invite_links(app: AppHandle) -> Result<Vec<String>, String> {
    Ok(app
        .deep_link()
        .get_current()
        .map_err(|e| e.to_string())?
        .unwrap_or_default()
        .into_iter()
        .map(|u| u.to_string())
        .collect())
}
