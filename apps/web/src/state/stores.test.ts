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
