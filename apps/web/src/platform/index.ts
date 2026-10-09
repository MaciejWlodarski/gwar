/**
 * Platform layer: lets the shared UI tell the browser from the Tauri desktop
 * app without importing Tauri eagerly. `@tauri-apps/api` is only loaded
 * (dynamically) once `isDesktop()` is true, so the web bundle never runs it.
 */

/** True inside the Tauri (v2) webview. */
export function isDesktop(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

export type Unlisten = () => void;

export interface TauriBridge {
  invoke<T = void>(command: string, args?: Record<string, unknown>): Promise<T>;
  listen<T>(event: string, handler: (payload: T) => void): Promise<Unlisten>;
}

let bridge: Promise<TauriBridge> | null = null;

/** The `invoke`/`listen` pair of the desktop shell. Only call when `isDesktop()`. */
export function tauri(): Promise<TauriBridge> {
  bridge ??= Promise.all([import("@tauri-apps/api/core"), import("@tauri-apps/api/event")]).then(([core, event]) => ({
    invoke: <T = void>(command: string, args?: Record<string, unknown>) => core.invoke<T>(command, args),
    listen: <T>(name: string, handler: (payload: T) => void) => event.listen<T>(name, (e) => handler(e.payload)),
  }));
  return bridge;
}
