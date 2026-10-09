import { isDesktop, tauri } from ".";

/**
 * Saves a server file. The browser downloads it (the server marks everything
 * but images as an attachment). The desktop webview does not handle downloads,
 * so there a native "Save as" dialog asks for the place and the app fetches the
 * file itself. Resolves to false if the person cancelled.
 */
export async function saveFile(url: string, name: string): Promise<boolean> {
  if (isDesktop()) return (await tauri()).invoke<boolean>("save_url_as", { url, fileName: name });
  const a = document.createElement("a");
  a.href = url;
  a.download = name;
  // Cross-origin links ignore `download`; keep the app's page in place if the browser shows the file instead.
  a.target = "_blank";
  a.rel = "noopener noreferrer";
  document.body.appendChild(a);
  a.click();
  a.remove();
  return true;
}

/**
 * Sends a file to a one-time upload URL from the desktop app. The webview's own
 * origin (`tauri://`) would need CORS on the server for a `PUT`; the app does the
 * request itself instead. Resolves to the HTTP status.
 */
export async function uploadFromApp(url: string, file: Blob): Promise<number> {
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<number>("upload_put", new Uint8Array(await file.arrayBuffer()), { headers: { "x-upload-url": url } });
}
