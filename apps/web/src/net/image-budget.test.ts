import { describe, expect, it } from "vitest";
import { ImageBudget } from "./image-budget";

const tick = () => new Promise((r) => setTimeout(r, 0));

describe("image budget", () => {
  it("runs a bounded number of fetches and admits the next in order on release", async () => {
    const budget = new ImageBudget({ fetches: 2, memory: 100 });
    const signal = new AbortController().signal;
    const order: number[] = [];
    const slots = [0, 1, 2, 3].map((i) => budget.fetchSlot(signal).then((release) => { order.push(i); return release; }));
    await tick();
    expect(order).toEqual([0, 1]);
    (await slots[0]!)(); (await slots[0]!)(); // releasing twice frees one slot
    await tick();
    expect(order).toEqual([0, 1, 2]);
    expect(budget.usage.fetching).toBe(2);
  });

  it("holds memory until released and refuses what can never fit", async () => {
    const budget = new ImageBudget({ fetches: 1, memory: 100 });
    const signal = new AbortController().signal;
    const first = await budget.reserve(60, signal);
    let second: (() => void) | undefined;
    void budget.reserve(60, signal).then((r) => { second = r; });
    await tick();
    expect(second).toBeUndefined();
    first();
    await tick();
    expect(second).toBeDefined(); expect(budget.usage.memory).toBe(60);
    await expect(budget.reserve(101, signal)).rejects.toThrow("memory budget");
  });

  it("drops an aborted waiter so it never holds anything", async () => {
    const budget = new ImageBudget({ fetches: 1, memory: 10 });
    const held = await budget.reserve(10, new AbortController().signal);
    const abort = new AbortController();
    const waiting = budget.reserve(5, abort.signal);
    abort.abort();
    await expect(waiting).rejects.toBeDefined();
    held();
    await tick();
    expect(budget.usage.memory).toBe(0);
  });
});
