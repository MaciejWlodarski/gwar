// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { useSettings } from "../state/settings";
import { ChangeRecoveryCode } from "./AccountSettings";
import { TooltipProvider } from "./kit";

const actions = vi.hoisted(() => ({ changeRecoveryCode: vi.fn(async (_password: string, _code: string) => {}) }));
vi.mock("../state/account", () => ({ accountActions: actions, useAccount: () => ({}) }));
vi.mock("../state/controller", () => ({ controller: {} }));

let container: HTMLDivElement, root: Root;
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  useSettings.setState({ language: "en" });
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
  document.body.innerHTML = "";
});

const button = (text: string) => [...document.querySelectorAll("button")].find((b) => b.textContent?.includes(text))!;

it("sets a new recovery code only after it was shown and kept", async () => {
  await act(async () => root.render(<TooltipProvider><ChangeRecoveryCode /></TooltipProvider>));
  await act(async () => button("Change recovery code").click());
  const code = document.querySelector("[data-testid=recovery-code]")!.textContent!;
  expect(code).toMatch(/^([A-Z2-7]{4}-){7}[A-Z2-7]{4}$/);
  const submit = [...document.querySelectorAll("button[type=submit]")][0] as HTMLButtonElement;
  expect(submit.disabled).toBe(true);
  const password = document.querySelector("input[type=password]") as HTMLInputElement;
  await act(async () => {
    const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
    set.call(password, "secret password");
    password.dispatchEvent(new Event("input", { bubbles: true }));
    (document.querySelector("input[type=checkbox]") as HTMLInputElement).click();
  });
  expect(submit.disabled).toBe(false);
  await act(async () => submit.click());
  expect(actions.changeRecoveryCode).toHaveBeenCalledWith("secret password", code);
  expect(document.querySelector("[data-testid=recovery-code]")).toBeNull();
});
