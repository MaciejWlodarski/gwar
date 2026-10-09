//! Files shared in chat. The webview does not save `Content-Disposition`
//! downloads, and (from the `tauri://` origin) it may not `PUT` to the server
//! either, so both go through the app: "Save as" asks where, then the app
//! fetches the file itself; uploads are sent from here.

use std::path::PathBuf;

use tauri::ipc::{InvokeBody, Request};
use tauri_plugin_dialog::DialogExt;
use tokio::io::AsyncWriteExt;

fn http_url(url: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(url).map_err(|e| e.to_string())?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        other => Err(format!("unsupported URL scheme {other}")),
    }
}

/// A name that is safe to propose in the dialog: no directories.
fn proposed_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name).trim();
    if base.is_empty() { "file".into() } else { base.to_owned() }
}

/// Asks where to save, downloads `url` there. `false`: the person cancelled.
#[tauri::command]
pub async fn save_url_as(app: tauri::AppHandle, url: String, file_name: String) -> Result<bool, String> {
    let url = http_url(&url)?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().set_file_name(proposed_name(&file_name)).save_file(move |path| {
        let _ = tx.send(path);
    });
    let Some(path) = rx.await.map_err(|e| e.to_string())? else { return Ok(false) };
    let path: PathBuf = path.into_path().map_err(|e| e.to_string())?;
    if let Err(e) = download(url, &path).await {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(e);
    }
    Ok(true)
}

async fn download(url: reqwest::Url, path: &PathBuf) -> Result<(), String> {
    let mut response =
        reqwest::get(url).await.map_err(|e| e.to_string())?.error_for_status().map_err(|e| e.to_string())?;
    let mut file = tokio::fs::File::create(path).await.map_err(|e| e.to_string())?;
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
    }
    file.flush().await.map_err(|e| e.to_string())
}

/// `PUT`s the raw request body to the URL in the `x-upload-url` header (the
/// one-time upload URL the server handed out). Answers with the HTTP status.
#[tauri::command]
pub async fn upload_put(request: Request<'_>) -> Result<u16, String> {
    let url = request.headers().get("x-upload-url").and_then(|v| v.to_str().ok()).ok_or("missing upload URL")?;
    let url = http_url(url)?;
    let InvokeBody::Raw(bytes) = request.body() else { return Err("expected raw bytes".into()) };
    let response = reqwest::Client::new().put(url).body(bytes.clone()).send().await.map_err(|e| e.to_string())?;
    Ok(response.status().as_u16())
}

#[cfg(test)]
mod tests {
    use super::{http_url, proposed_name};

    #[test]
    fn only_http_urls_are_fetched() {
        assert!(http_url("https://voice.example.com/files/a/b.png").is_ok());
        assert!(http_url("http://127.0.0.1:8790/files/a/b.png").is_ok());
        assert!(http_url("file:///etc/passwd").is_err());
        assert!(http_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn proposed_names_have_no_directories() {
        assert_eq!(proposed_name("../../etc/passwd"), "passwd");
        assert_eq!(proposed_name("C:\\Users\\x\\a.txt"), "a.txt");
        assert_eq!(proposed_name("photo.png"), "photo.png");
        assert_eq!(proposed_name("  "), "file");
    }
}
