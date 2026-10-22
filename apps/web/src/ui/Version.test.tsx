// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { useUpdate } from "../platform/updater";
import { initialScreen, useScreen } from "../state/screen";
import { useSettings } from "../state/settings";
import { useUi, useVoice } from "../state/stores";
import { TooltipProvider } from "./kit";
import { UpdateButton, VersionLabel } from "./Version";

const actions = vi.hoisted(() => ({ installUpdate: vi.fn(), retryUpdate: vi.fn() }));
vi.mock("../platform/updater", async (original) => ({ ...(await original<typeof import("../platform/updater")>()), ...actions }));

let container: HTMLDivElement, root: Root;
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  useSettings.setState({ language: "en" });
  useUpdate.setState({ status: { phase: "none" } });
  useVoice.setState({ state: "idle" });
  useScreen.setState(initialScreen);
  useUi.setState({ dialog: { kind: "none" } });
  actions.installUpdate.mockClear();
  actions.retryUpdate.mockClear();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
  document.body.innerHTML = "";
  vi.unstubAllGlobals();
});

const render = () => act(async () => root.render(<TooltipProvider><UpdateButton /></TooltipProvider>));
const button = () => container.querySelector("button");

it("shows nothing while the app is up to date", async () => {
  await render();
  expect(button()).toBeNull();
});

it("shows the download progress and ignores clicks meanwhile", async () => {
  useUpdate.setState({ status: { phase: "downloading", version: "0.1.3", progress: 0.42 } });
  await render();
  expect(button()!.getAttribute("aria-label")).toBe("Downloading Gwar 0.1.3: 42%");
  await act(async () => button()!.click());
  expect(actions.installUpdate).not.toHaveBeenCalled();
});

it("installs a downloaded update on click", async () => {
  useUpdate.setState({ status: { phase: "ready", version: "0.1.3" } });
  await render();
  await act(async () => button()!.click());
  expect(actions.installUpdate).toHaveBeenCalledTimes(1);
  expect(useUi.getState().dialog.kind).toBe("none");
});

it.each([
  ["in a call", () => useVoice.setState({ state: "connected" })],
  ["while sharing the screen", () => useScreen.setState({ watches: [{ share: "s", id: 1, slot: 0, status: "watching" }] })],
])("asks first %s, because the restart ends it", async (_, setUp) => {
  setUp();
  useUpdate.setState({ status: { phase: "ready", version: "0.1.3" } });
  await render();
  await act(async () => button()!.click());
  expect(actions.installUpdate).not.toHaveBeenCalled();
  const dialog = useUi.getState().dialog;
  expect(dialog).toMatchObject({ kind: "confirm", title: "Restart Gwar now?", confirmLabel: "Restart and update" });
  if (dialog.kind === "confirm") dialog.onConfirm();
  expect(actions.installUpdate).toHaveBeenCalledTimes(1);
});

it("retries after a failure", async () => {
  useUpdate.setState({ status: { phase: "failed", version: "0.1.3", error: "offline" } });
  await render();
  expect(button()!.getAttribute("aria-label")).toBe("Could not update to Gwar 0.1.3: offline. Click to try again.");
  await act(async () => button()!.click());
  expect(actions.retryUpdate).toHaveBeenCalledTimes(1);
});

it("copies the version with the commit it was built from", async () => {
  const writeText = vi.spyOn(navigator.clipboard, "writeText").mockResolvedValue();
  await act(async () => root.render(<TooltipProvider><VersionLabel /></TooltipProvider>));
  expect(button()!.textContent).toBe(`v${__APP_VERSION__}${__APP_REVISION__}`);
  await act(async () => button()!.click());
  expect(writeText).toHaveBeenCalledWith(`v${__APP_VERSION__} · ${__APP_REVISION__}`);
  expect(__APP_REVISION__).toMatch(/^([0-9a-f]{7}(-dirty)?|unknown)$/);
});
