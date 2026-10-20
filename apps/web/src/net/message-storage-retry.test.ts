import "fake-indexeddb/auto";
import { afterEach, expect, it, vi } from "vitest";
import { FEED_RETRIES, messageStorage, refreshRevocations } from "./message-storage";

afterEach(() => { vi.useRealTimers(); vi.restoreAllMocks(); });

it("reads the feed again by itself after a failure: an idle view recovers without new messages or a forced read", async () => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
  vi.spyOn(console, "warn").mockImplementation(() => {});
  const fetch = vi.spyOn(globalThis, "fetch").mockImplementation(async () => new Response("down", { status: 503 }));
  const recovered = vi.fn();
  messageStorage.subscribe(recovered);
  await refreshRevocations();
  expect(messageStorage.feedError).toBe("Error: Revocation feed unavailable");
  // Failing on: 10, 20 and 40 s later, then nothing more (the callers' minute takes over).
  for (const delay of FEED_RETRIES) {
    await vi.advanceTimersByTimeAsync(delay - 1);
    const before = fetch.mock.calls.length;
    await vi.advanceTimersByTimeAsync(1);
    await vi.waitFor(() => expect(fetch.mock.calls.length).toBe(before + 1));
  }
  const calls = fetch.mock.calls.length;
  await vi.advanceTimersByTimeAsync(10 * 60_000);
  expect(fetch.mock.calls.length).toBe(calls);
  expect(recovered).not.toHaveBeenCalled();
  // The next failure streak starts after a success; a read that works recovers by itself.
  fetch.mockImplementation(async () => Response.json({ revocations: [] }));
  await refreshRevocations();
  expect(recovered).toHaveBeenCalledOnce();
  fetch.mockImplementationOnce(async () => new Response("down", { status: 503 }));
  await vi.advanceTimersByTimeAsync(60_000);
  await refreshRevocations();
  expect(messageStorage.feedError).toBeDefined();
  await vi.advanceTimersByTimeAsync(FEED_RETRIES[0]!);
  await vi.waitFor(() => expect(messageStorage.feedError).toBeUndefined());
  expect(recovered).toHaveBeenCalledTimes(2);
  expect((await messageStorage.revocation("a", "b")).fresh).toBe(true);
});
