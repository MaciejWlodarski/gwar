// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ServerInfo } from "../proto/ServerInfo";
import { useSettings } from "../state/settings";
import { useSession } from "../state/stores";
import { TsGroupBadges } from "./badges";

const server: ServerInfo = {
  name: "TS", welcome: "", version: "3", default_channel: 1, max_clients: 32, upload_limit: 0, file_access: "link", teamspeak: true,
  server_chat: { encrypted: false, history: false },
  badges: [
    { kind: "server", id: 6, name: "Server Admin", sort: 0 },
    { kind: "server", id: 9, name: "<b>Loud</b>", sort: 5 },
    { kind: "channel", id: 5, name: "Channel Admin", sort: 0 },
  ],
};

describe("TeamSpeak group badges", () => {
  let container: HTMLDivElement, root: Root;
  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    useSettings.setState({ language: "en" });
    useSession.setState({ kind: "teamspeak", server });
    container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  });
  afterEach(async () => {
    await act(async () => root.unmount()); container.remove(); vi.unstubAllGlobals();
  });

  it("names someone's server groups, then their channel group, as text", async () => {
    await act(async () => root.render(<TsGroupBadges groups={[9, 6, 8]} channelGroup={5} />));
    const names = [...container.querySelectorAll("[data-testid=ts-group-badges] > span")].map((s) => s.textContent);
    expect(names).toEqual(["Server Admin", "<b>Loud</b>", "Channel Admin"]);
    expect(container.querySelector("b")).toBeNull();
  });

  it("shows nothing for someone in no badge group", async () => {
    await act(async () => root.render(<TsGroupBadges groups={[8]} channelGroup={7} />));
    expect(container.querySelector("[data-testid=ts-group-badges]")).toBeNull();
  });
});
