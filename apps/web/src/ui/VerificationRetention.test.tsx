// @vitest-environment happy-dom
// Gate regression (Astra r2 probe, inverted): the view keeps verification results only for rows the thread holds.
import "fake-indexeddb/auto";
import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import { useSession } from "../state/stores";
import { initialState } from "../state/reducer";
import { MessageList } from "./Chat";
import { TooltipProvider } from "./kit";

const observed = vi.hoisted(() => ({ map: new Map<number, { fingerprint: string }>() }));
vi.mock("react", async (get) => {
  const original = await get<typeof import("react")>();
  return { ...original, useState: (...args: unknown[]) => {
    const pair = (original.useState as (...a: unknown[]) => [unknown, unknown])(...args);
    if (pair[0] instanceof Map) observed.map = pair[0] as typeof observed.map;
    return pair;
  } };
});
vi.mock("./useVerifiedHandle", () => ({ useVerifiedHandle: () => null }));
vi.mock("../state/controller", () => ({ controller: { messageServerKey: "S", messageUid: Promise.resolve("me") }, UploadError: class extends Error {} }));

it("drops verification results of rows the thread evicted, including late completions", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  useSession.setState({ ...initialState, kind: "vc", me: { uid: "me", session: 1 }, viewChannel: 1 });
  const div = document.createElement("div"); document.body.appendChild(div);
  const root = createRoot(div);
  const item = (id: number) => ({ id, target: { channel: 1 }, author: 2, author_uid: "u", author_name: "attacker", text: `unique-${id}`, sent_at: 1700000000000 });
  const settle = () => act(async () => { for (let i = 0; i < 5; i++) await new Promise((r) => setTimeout(r, 20)); });
  try {
    await act(async () => root.render(<TooltipProvider><MessageList threadKey="channel" /></TooltipProvider>));
    for (let batch = 0; batch < 2; batch++) {
      await act(async () => {
        for (let n = 1; n <= 1000; n++) useSession.getState().dispatch({ type: "event", now: 1, event: { ev: "chat.message", d: item(batch * 1000 + n) } });
      });
      await settle();
    }
    expect(useSession.getState().threads["ch:1"]!.items).toHaveLength(1000);
    expect(observed.map.size).toBe(1000);
    expect(observed.map.has(1)).toBe(false);
    expect(observed.map.has(2000)).toBe(true);
  } finally { await act(async () => root.unmount()); div.remove(); vi.unstubAllGlobals(); }
});
