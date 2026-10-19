// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Channel } from "../proto/Channel";
import type { Group } from "../proto/Group";
import { useSettings } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import { Dialogs } from "./Dialogs";
import { TooltipProvider } from "./kit";

vi.mock("../state/controller", () => ({ controller: { onRoster: () => () => {} }, describeRequestError: String, UploadError: class extends Error {} }));

const group = (id: number, name: string, position: number, permissions: Group["permissions"]) =>
  ({ id, name, permissions, color: null, position, hoist: false, mentionable: false }) as unknown as Group;
const channel = (id: number, name: string, over: Partial<Channel> = {}) =>
  ({ id, parent: null, name, topic: "", position: id, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false, ...over }) as Channel;

describe("view as", () => {
  let container: HTMLDivElement, root: Root;
  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    useSettings.setState({ language: "en" });
    useSession.setState({
      kind: "vc",
      groups: { 1: group(1, "Admin", 0, ["administrator"]), 5: group(5, "Speakers", 1, []), 2: group(2, "Member", 2, ["connect", "send_messages"]) },
      channels: {
        1: channel(1, "Lobby"),
        3: channel(3, "Stage", { overrides: [{ role: 5, allow: ["speak"], deny: [] }] }),
        4: channel(4, "Crew", { private: true, access: { roles: [5], users: [] } }),
      },
      members: { alice: { uid: "alice", nickname: "Alice", tag: "a", groups: [2], last_seen: 0 } as never },
      owner: null as never,
    });
    container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  });
  afterEach(async () => {
    await act(async () => root.unmount()); container.remove(); useUi.getState().closeDialog(); vi.unstubAllGlobals();
  });
  const rows = () => [...document.querySelectorAll("tbody tr")].map((tr) => [tr.querySelector("td")!.textContent, tr.querySelectorAll('[aria-label="Yes"]').length]);

  it("shows a role's channels and what it may do, and a member's own view", async () => {
    useUi.getState().openDialog({ kind: "viewAs", role: 5 });
    await act(async () => root.render(<TooltipProvider><Dialogs /></TooltipProvider>));
    expect(document.body.textContent).toContain("View as Speakers");
    // Speakers see the private Crew and may speak on the Stage (join, speak, write).
    expect(rows()).toEqual([["Lobby", 2], ["Stage", 3], ["Crew", 2]]);
    useUi.getState().openDialog({ kind: "viewAs", uid: "alice" });
    await act(async () => root.render(<TooltipProvider><Dialogs /></TooltipProvider>));
    expect(document.body.textContent).toContain("View as Alice");
    expect(rows()).toEqual([["Lobby", 2], ["Stage", 2]]);
    expect(document.body.textContent).toContain("1 private channel(s) you can see are hidden from them.");
  });
});
