// @vitest-environment happy-dom
import { expect, it, vi } from "vitest";

const session = vi.hoisted(() => ({ start: vi.fn(), close: vi.fn() }));
vi.mock("./session", async importOriginal => ({
  ...await importOriginal<typeof import("./session")>(),
  WatchSession: class { start = session.start; close = session.close; },
}));

it("reloads a bfcache-restored watch page after pagehide closed its session", async () => {
  document.body.innerHTML = '<main id="watch"><h1 id="title"></h1><div id="status"></div><video id="video"></video><button id="fullscreen"></button><button id="pip"></button></main>';
  const listen = vi.spyOn(window, "addEventListener");
  const reload = vi.spyOn(location, "reload").mockImplementation(() => {});
  try {
    await import("./main");
    expect(session.start).toHaveBeenCalledTimes(1);
    window.dispatchEvent(Object.assign(new Event("pageshow"), { persisted: false }));
    expect(reload).not.toHaveBeenCalled();
    window.dispatchEvent(new Event("pagehide"));
    expect(session.close).toHaveBeenCalledTimes(1);
    window.dispatchEvent(Object.assign(new Event("pageshow"), { persisted: true }));
    expect(reload).toHaveBeenCalledTimes(1);
  } finally {
    for (const [type, listener] of listen.mock.calls) window.removeEventListener(type, listener);
    listen.mockRestore(); reload.mockRestore(); document.body.replaceChildren();
  }
});
