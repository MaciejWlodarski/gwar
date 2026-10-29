// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useAccount, accountActions } from "../state/account";
import { exportIdentityBackup } from "../state/identity-backup";
import { useSettings } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import { IdentityNotices } from "./IdentityNotices";
vi.mock("../state/controller", () => ({ controller: {} }));
vi.mock("../state/identity-backup", () => ({ exportIdentityBackup: vi.fn() }));
let root: Root, container: HTMLDivElement;
const account = { handle: "alex", accountKey: "account", deviceKey: "device", deviceName: "Browser", expiresAt: 0 };
const day = 86_400_000;
beforeEach(async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.useFakeTimers(); vi.setSystemTime(new Date("2026-10-29T09:00:00Z"));
  useSettings.setState({ language: "pl", onboarding: { completed: true, firstUsedAt: Date.now(), nudgeDismissed: false, roleReceived: false } });
  useAccount.setState({ loaded: true, certChecked: true, account: null });
  useSession.setState({ kind: "gwar", me: null, members: {} });
  vi.mocked(exportIdentityBackup).mockReset();
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  await act(async () => root.render(<IdentityNotices />));
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); vi.restoreAllMocks(); vi.useRealTimers(); vi.unstubAllGlobals(); });
const button = (label: string) => Array.from(document.querySelectorAll("button")).find(b => b.textContent === label)!;
describe("persistent identity notices", () => {
  it("shows the reminder on role receipt and remembers dismissal", async () => {
    await act(async () => useSession.setState({ me: { uid: "me", session: 1 }, members: { me: { uid: "me", nickname: "Alex", tag: "alex", groups: [1, 2], last_seen: 1 } } }));
    expect(container.querySelector('[data-testid="identity-banner"]')).not.toBeNull();
    await act(async () => (container.querySelector('[aria-label="Zamknij"]') as HTMLButtonElement).click());
    expect(useSettings.getState().onboarding?.nudgeDismissed).toBe(true);
    expect(container.textContent).toBe("");
    await act(async () => root.render(<IdentityNotices />));
    expect(container.textContent).toBe("");
  });
  it("does not treat the default Member role as receiving a role", async () => {
    await act(async () => useSession.setState({ me: { uid: "me", session: 1 }, members: { me: { uid: "me", nickname: "Alex", tag: "alex", groups: [2], last_seen: 1 } } }));
    expect(container.textContent).toBe("");
  });
  it("offers account creation and backup; signed-in accounts hide the reminder", async () => {
    await act(async () => useSettings.getState().setOnboarding({ roleReceived: true }));
    await act(async () => button("Załóż konto").click());
    expect(useUi.getState()).toMatchObject({ section: "settings", settingsTab: "account", accountEntry: "create" });
    await act(async () => button("Eksportuj kopię").click());
    expect(exportIdentityBackup).toHaveBeenCalledOnce();
    await act(async () => useAccount.setState({ account: { ...account, expiresAt: Date.now() + 90 * day } }));
    expect(container.textContent).toBe("");
  });
  it("waits for the startup certificate check, persists, reaches expiry and opens the password dialog", async () => {
    await act(async () => useAccount.setState({ account: { ...account, expiresAt: Date.now() + 5 * day }, certChecked: false }));
    expect(container.textContent).toBe("");
    await act(async () => useAccount.setState({ certChecked: true }));
    expect(container.textContent).toContain("Za 5 dni trzeba będzie ponownie wpisać hasło na tym urządzeniu");
    await act(async () => { vi.setSystemTime(Date.now() + 5 * day); window.dispatchEvent(new Event("focus")); });
    expect(container.textContent).toContain("Wpisz ponownie hasło");
    await act(async () => button("Zaloguj ponownie").click());
    expect(document.querySelector('[type="password"]')).not.toBeNull();
    const renew = vi.spyOn(accountActions, "renew").mockImplementation(async () => { useAccount.setState({ account: { ...account, expiresAt: Date.now() + 90 * day } }); });
    await act(async () => document.querySelector('[role="dialog"] form')!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
    expect(renew).toHaveBeenCalledOnce();
    expect(container.querySelector('[data-testid="expiry-banner"]')).toBeNull();
  });
});
