/**
 * Desktop app updates (docs/releasing.md, "Updates"). Shortly after startup and then every few
 * hours the app asks the release feed for a newer version and downloads it in the background;
 * nothing is installed until the user clicks the update button in the server rail
 * (ui/UpdateButton.tsx), which installs it and restarts the app. The Tauri updater plugin
 * verifies the download against the release key before installing it.
 *
 * Only the built desktop app loads the updater and process plugins (`initUpdates`).
 */
import { create } from "zustand";
import { isDesktop } from ".";

/** No update known (up to date, or the feed could not be read: that is retried quietly). */
interface None { phase: "none" }
/** `progress` from 0 to 1, or null while the size is unknown. */
interface Downloading { phase: "downloading"; version: string; progress: number | null }
interface Ready { phase: "ready"; version: string }
interface Installing { phase: "installing"; version: string }
/** Downloading or installing `version` failed; a retry checks and downloads again. */
interface Failed { phase: "failed"; version: string; error: string }
export type UpdateStatus = None | Downloading | Ready | Installing | Failed;

export const useUpdate = create<{ status: UpdateStatus }>()(() => ({ status: { phase: "none" } }));

export type DownloadEvent =
  | { event: "Started"; data: { contentLength?: number } }
  | { event: "Progress"; data: { chunkLength: number } }
  | { event: "Finished" };

/** The part of the plugin's `Update` used here: a newer release the feed announced. */
export interface PendingUpdate {
  readonly version: string;
  download(onEvent: (e: DownloadEvent) => void, options?: { timeout?: number }): Promise<void>;
  /** Installs the downloaded update. On Windows the installer takes over and the app exits. */
  install(): Promise<void>;
  /** Frees the plugin's copy of the update and of its download. */
  close(): Promise<void>;
}

export interface UpdateBackend {
  /** The newer release, or null when this version is the latest. */
  check(): Promise<PendingUpdate | null>;
  relaunch(): Promise<void>;
}

export const FIRST_CHECK_MS = 15_000;
export const CHECK_EVERY_MS = 6 * 60 * 60_000;
/** A download that has not finished by then is abandoned and tried again later. */
export const DOWNLOAD_TIMEOUT_MS = 30 * 60_000;

function describe(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** The update cycle. One step runs at a time; a check while one runs is skipped. */
export class Updater {
  private pending: PendingUpdate | null = null;
  private busy = false;

  constructor(
    private readonly backend: UpdateBackend,
    private readonly store = useUpdate,
  ) {}

  get status(): UpdateStatus {
    return this.store.getState().status;
  }

  private set(status: UpdateStatus): void {
    this.store.setState({ status });
  }

  /** Checks the feed after `FIRST_CHECK_MS`, and then every `CHECK_EVERY_MS`. */
  start(): () => void {
    const first = setTimeout(() => void this.check(), FIRST_CHECK_MS);
    const every = setInterval(() => void this.check(), CHECK_EVERY_MS);
    return () => {
      clearTimeout(first);
      clearInterval(every);
    };
  }

  /** Asks the feed and downloads a newer version than the one already downloaded, if any. */
  async check(): Promise<void> {
    if (this.busy) return;
    this.busy = true;
    try {
      let update: PendingUpdate | null;
      try {
        update = await this.backend.check();
      } catch (e) {
        // Offline, or no published release yet: nothing to show, the next check tries again.
        console.warn("[update] check failed:", describe(e));
        return;
      }
      const now = this.status;
      if (update && now.phase === "ready" && now.version === update.version) {
        void update.close().catch(() => {});
        return;
      }
      await this.drop();
      if (!update) {
        this.set({ phase: "none" });
        return;
      }
      this.pending = update;
      await this.download(update);
    } finally {
      this.busy = false;
    }
  }

  private async download(update: PendingUpdate): Promise<void> {
    const { version } = update;
    let total: number | null = null;
    let received = 0;
    this.set({ phase: "downloading", version, progress: null });
    try {
      await update.download((e) => {
        if (e.event === "Started") total = e.data.contentLength || null;
        if (e.event !== "Progress" || total === null) return;
        received += e.data.chunkLength;
        // Whole percents only: a download sends thousands of chunks.
        const progress = Math.min(1, Math.floor((received / total) * 100) / 100);
        const now = this.status;
        if (now.phase !== "downloading" || now.progress !== progress) this.set({ phase: "downloading", version, progress });
      }, { timeout: DOWNLOAD_TIMEOUT_MS });
      this.set({ phase: "ready", version });
    } catch (e) {
      console.warn(`[update] downloading ${version} failed:`, describe(e));
      this.set({ phase: "failed", version, error: describe(e) });
    }
  }

  /** Installs the downloaded update and restarts the app. */
  async install(): Promise<void> {
    const now = this.status;
    if (this.busy || now.phase !== "ready" || !this.pending) return;
    this.busy = true;
    this.set({ phase: "installing", version: now.version });
    try {
      await this.pending.install();
      await this.backend.relaunch();
    } catch (e) {
      console.warn(`[update] installing ${now.version} failed:`, describe(e));
      this.set({ phase: "failed", version: now.version, error: describe(e) });
    } finally {
      this.busy = false;
    }
  }

  /** After a failure: checks and downloads again. */
  retry(): Promise<void> {
    return this.check();
  }

  private async drop(): Promise<void> {
    const pending = this.pending;
    this.pending = null;
    await pending?.close().catch(() => {});
  }
}

let updater: Updater | null = null;

/**
 * Starts the update checks in a built desktop app; returns their cleanup. Other builds leave the
 * plugins out: the web app has none, and `tauri dev` must not replace itself with a release (debug
 * desktop builds have no updater plugin at all, lib.rs, so their checks fail quietly).
 */
export function initUpdates(): (() => void) | undefined {
  if (import.meta.env.MODE !== "desktop" || !isDesktop()) return undefined;
  updater ??= new Updater({
    check: async () => (await import("@tauri-apps/plugin-updater")).check({ timeout: 30_000 }),
    relaunch: async () => (await import("@tauri-apps/plugin-process")).relaunch(),
  });
  return updater.start();
}

export function installUpdate(): void {
  void updater?.install();
}

export function retryUpdate(): void {
  void updater?.retry();
}
