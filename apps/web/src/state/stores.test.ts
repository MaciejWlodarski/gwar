import { expect, it } from "vitest";
import { useSession, useUi } from "./stores";

it("opening a conversation leaves the overview and the home section for the chat", () => {
  const ui = () => useUi.getState();
  ui().showOverview();
  expect(ui()).toMatchObject({ section: "servers", overview: true });

  ui().setSection("home");
  useSession.getState().dispatch({ type: "setActive", key: "server" });
  expect(ui()).toMatchObject({ section: "servers", overview: false });

  ui().showOverview();
  useSession.getState().dispatch({ type: "openDm", uid: "u1", name: "Ola" });
  expect(ui().overview).toBe(false);

  // Session events that are not someone opening a conversation keep the overview.
  ui().showOverview();
  useSession.getState().dispatch({ type: "focus", focused: true });
  expect(ui().overview).toBe(true);
});

it("the settings and bug report pages go back to where they were opened from", () => {
  const ui = () => useUi.getState();
  ui().setSection("home");
  ui().openSettings("identity");
  expect(ui()).toMatchObject({ section: "settings", settingsTab: "identity", back: "home" });
  // The gear reopens its last tab, not a profile one; moving between the pages keeps the way back.
  ui().openSettings();
  expect(ui()).toMatchObject({ section: "settings", settingsTab: "audio", back: "home" });
  ui().setSettingsTab("appearance");
  ui().setSettingsTab("account");
  ui().setSection("bugReport");
  ui().openSettings();
  expect(ui()).toMatchObject({ section: "settings", settingsTab: "appearance", back: "home" });
  ui().goBack();
  expect(ui().section).toBe("home");

  ui().showChat();
  ui().setSection("bugReport");
  ui().goBack();
  expect(ui()).toMatchObject({ section: "servers", overview: false });
});
