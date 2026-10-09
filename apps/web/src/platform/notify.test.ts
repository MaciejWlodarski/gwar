import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const desktop = vi.hoisted(() => ({ on: false }));
vi.mock(".", () => ({ isDesktop: () => desktop.on }));

const plugin = vi.hoisted(() => ({
  isPermissionGranted: vi.fn<() => Promise<boolean>>(),
  requestPermission: vi.fn<() => Promise<string>>(),
  sendNotification: vi.fn(),
}));
vi.mock("@tauri-apps/plugin-notification", () => plugin);

import { DESKTOP_CLICK_WINDOW_MS, notificationsPermission, notify, requestNotifications } from "./notify";

class FakeNotification {
  static permission: NotificationPermission = "default";
  static requestPermission = vi.fn(async () => FakeNotification.permission);
  static shown: FakeNotification[] = [];
  onclick: (() => void) | null = null;
  close = vi.fn();
  constructor(
    readonly title: string,
    readonly options?: NotificationOptions,
  ) {
    FakeNotification.shown.push(this);
  }
}

const listeners = new Map<string, () => void>();
const fakeWindow = {
  focus: vi.fn(),
  addEventListener: (type: string, fn: () => void) => listeners.set(type, fn),
  removeEventListener: (type: string) => listeners.delete(type),
};

beforeEach(() => {
  desktop.on = false;
  FakeNotification.permission = "default";
  FakeNotification.shown = [];
  FakeNotification.requestPermission.mockClear();
  listeners.clear();
  fakeWindow.focus.mockClear();
  vi.stubGlobal("Notification", FakeNotification);
  vi.stubGlobal("window", fakeWindow);
  vi.stubGlobal("document", { hasFocus: () => false });
  plugin.isPermissionGranted.mockReset();
  plugin.requestPermission.mockReset();
  plugin.sendNotification.mockReset();
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("browser", () => {
  it("reports the Notification permission", async () => {
    expect(await notificationsPermission()).toBe("default");
    FakeNotification.permission = "granted";
    expect(await notificationsPermission()).toBe("granted");
    vi.stubGlobal("Notification", undefined);
    expect(await notificationsPermission()).toBe("denied");
  });

  it("only asks when the permission is undecided", async () => {
    FakeNotification.requestPermission.mockResolvedValueOnce("granted");
    expect(await requestNotifications()).toBe(true);
    expect(FakeNotification.requestPermission).toHaveBeenCalledTimes(1);
    FakeNotification.permission = "denied";
    expect(await requestNotifications()).toBe(false);
    expect(FakeNotification.requestPermission).toHaveBeenCalledTimes(1);
  });

  it("does not show anything without permission", () => {
    notify({ title: "a", body: "b" });
    expect(FakeNotification.shown).toHaveLength(0);
  });

  it("shows with title, body and tag; a click focuses the window and calls back", () => {
    FakeNotification.permission = "granted";
    const onClick = vi.fn();
    notify({ title: "Anna", body: "hi @you", tag: "chan-1", onClick });
    const [n] = FakeNotification.shown;
    expect(n?.title).toBe("Anna");
    expect(n?.options).toEqual({ body: "hi @you", tag: "chan-1" });
    n?.onclick?.();
    expect(fakeWindow.focus).toHaveBeenCalled();
    expect(n?.close).toHaveBeenCalled();
    expect(onClick).toHaveBeenCalledOnce();
  });

  it("works without an onClick callback", () => {
    FakeNotification.permission = "granted";
    notify({ title: "a", body: "b" });
    expect(() => FakeNotification.shown[0]?.onclick?.()).not.toThrow();
  });
});

describe("desktop", () => {
  beforeEach(() => {
    desktop.on = true;
  });

  it("maps the plugin permission", async () => {
    plugin.isPermissionGranted.mockResolvedValue(true);
    expect(await notificationsPermission()).toBe("granted");
    plugin.isPermissionGranted.mockResolvedValue(false);
    expect(await notificationsPermission()).toBe("default");
    FakeNotification.permission = "denied";
    expect(await notificationsPermission()).toBe("denied");
  });

  it("requests through the plugin", async () => {
    plugin.requestPermission.mockResolvedValue("granted");
    expect(await requestNotifications()).toBe(true);
    plugin.requestPermission.mockResolvedValue("denied");
    expect(await requestNotifications()).toBe(false);
  });

  it("sends through the plugin only when permitted", async () => {
    plugin.isPermissionGranted.mockResolvedValue(false);
    notify({ title: "a", body: "b" });
    await vi.waitFor(() => expect(plugin.isPermissionGranted).toHaveBeenCalled());
    await Promise.resolve();
    expect(plugin.sendNotification).not.toHaveBeenCalled();

    plugin.isPermissionGranted.mockResolvedValue(true);
    notify({ title: "a", body: "b" });
    await vi.waitFor(() => expect(plugin.sendNotification).toHaveBeenCalledWith({ title: "a", body: "b" }));
  });

  it("treats a focus right after a background notification as a click, once", async () => {
    plugin.isPermissionGranted.mockResolvedValue(true);
    const onClick = vi.fn();
    notify({ title: "a", body: "b", onClick });
    await vi.waitFor(() => expect(listeners.has("focus")).toBe(true));
    listeners.get("focus")?.();
    expect(onClick).toHaveBeenCalledOnce();
    expect(listeners.has("focus")).toBe(false);
  });

  it("stops waiting for a click after a while", async () => {
    vi.useFakeTimers();
    plugin.isPermissionGranted.mockResolvedValue(true);
    notify({ title: "a", body: "b", onClick: vi.fn() });
    await vi.advanceTimersByTimeAsync(0);
    expect(listeners.has("focus")).toBe(true);
    await vi.advanceTimersByTimeAsync(DESKTOP_CLICK_WINDOW_MS + 1);
    expect(listeners.has("focus")).toBe(false);
  });

  it("does not wait for a click while the app is already focused", async () => {
    vi.stubGlobal("document", { hasFocus: () => true });
    plugin.isPermissionGranted.mockResolvedValue(true);
    notify({ title: "a", body: "b", onClick: vi.fn() });
    await vi.waitFor(() => expect(plugin.sendNotification).toHaveBeenCalled());
    expect(listeners.has("focus")).toBe(false);
  });
});
