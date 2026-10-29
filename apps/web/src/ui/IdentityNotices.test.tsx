// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as platform from "../platform";
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
  useUi.setState({ toasts: [] });
  vi.stubGlobal("matchMedia", () => ({ matches: false, addEventListener() {}, removeEventListener() {} }));
  vi.mocked(exportIdentityBackup).mockReset();
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  await act(async () => root.render(<IdentityNotices />));
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); vi.restoreAllMocks(); vi.useRealTimers(); vi.unstubAllGlobals(); });
const button = (label: string) => Array.from(document.querySelectorAll("button")).find(b => b.textContent === label)!;
async function onPhone() {
  vi.stubGlobal("matchMedia", () => ({ matches: true, addEventListener() {}, removeEventListener() {} }));
  await act(async () => root.render(<IdentityNotices key="phone" />));
}
describe("persistent identity notices", () => {
  it("returns keyboard focus to Protect after Escape and backup cancellation", async () => {
    await onPhone();
    await act(async () => useSettings.getState().setOnboarding({ roleReceived: true }));
    const trigger = button("Zabezpiecz");
    await act(async () => { trigger.focus(); trigger.click(); });
    await act(async () => document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })));
    await act(async () => vi.advanceTimersByTimeAsync(1));
    expect(document.activeElement).toBe(trigger);
    await act(async () => trigger.click());
    await act(async () => button("Zapisz kopię").click());
    await act(async () => button("Anuluj").click());
    await act(async () => vi.advanceTimersByTimeAsync(1));
    expect(document.activeElement).toBe(trigger);
  });
  it("focuses a page control after a saved backup removes the banner, skipping hidden file inputs", async () => {
    const main = document.createElement("main"), file = document.createElement("input"), composer = document.createElement("textarea");
    file.type = "file"; file.hidden = true; main.append(file, composer); document.body.appendChild(main);
    // happy-dom has no layout; model the visible page and composer only.
    for (const element of [main, composer]) vi.spyOn(element, "getClientRects").mockReturnValue({ length: 1 } as DOMRectList);
    vi.spyOn(file, "getClientRects").mockReturnValue({ length: 0 } as DOMRectList);
    try {
      vi.mocked(exportIdentityBackup).mockImplementation(async () => { useSettings.getState().setOnboarding({ nudgeDismissed: true }); });
      await act(async () => useSettings.getState().setOnboarding({ roleReceived: true }));
      await act(async () => button("Zapisz kopię").click());
      await act(async () => button("Zapisz").click());
      await act(async () => vi.advanceTimersByTimeAsync(1));
      expect(container.querySelector('[data-testid="identity-banner"]')).toBeNull();
      expect(document.activeElement).toBe(composer);
    } finally { main.remove(); }
  });
  it.each([
    { language: "pl" as const, desktop: false, banner: "Zabezpiecz swoją tożsamość — bez kopii stracisz nazwę i role po wyczyszczeniu danych przeglądarki.", loss: "Jeśli wyczyścisz dane przeglądarki, stracisz swoją nazwę i role." },
    { language: "pl" as const, desktop: true, banner: "Zabezpiecz swoją tożsamość — bez kopii stracisz nazwę i role po usunięciu danych Gwar.", loss: "Jeśli usuniesz dane Gwar z tego komputera, stracisz swoją nazwę i role." },
    { language: "en" as const, desktop: false, banner: "Protect your identity — without a backup, clearing browser data will erase your name and roles.", loss: "If you clear browser data, you’ll lose your name and roles." },
    { language: "en" as const, desktop: true, banner: "Protect your identity — without a backup, removing Gwar’s data will erase your name and roles.", loss: "If you remove Gwar’s data from this computer, you’ll lose your name and roles." },
  ])("uses $language identity wording for desktop=$desktop", async ({ language, desktop, banner, loss }) => {
    vi.spyOn(platform, "isDesktop").mockReturnValue(desktop);
    await act(async () => { useSettings.setState({ language }); useSettings.getState().setOnboarding({ roleReceived: true }); });
    expect(container.querySelector('[data-testid="identity-banner"] p')?.textContent).toBe(banner);
    await onPhone();
    await act(async () => button(language === "pl" ? "Zabezpiecz" : "Protect").click());
    expect(document.querySelector('[role="dialog"]')?.textContent).toContain(loss);
  });
  it("opens mobile protection choices without exporting, then reaches the private-key confirmation", async () => {
    await onPhone();
    await act(async () => useSettings.getState().setOnboarding({ roleReceived: true }));
    const banner = container.querySelector('[data-testid="identity-banner"]')!;
    expect(banner.querySelector("p")?.textContent).toBe("Zabezpiecz swoją tożsamość");
    expect(Array.from(banner.querySelectorAll("button"), b => b.textContent)).toEqual(["Zabezpiecz", ""]);
    await act(async () => button("Zabezpiecz").click());
    const choices = document.querySelector('[role="dialog"]')!;
    expect(choices.textContent).toContain("Jeśli wyczyścisz dane przeglądarki, stracisz swoją nazwę i role.");
    expect(button("Załóż konto Gwar Connect")).toBeDefined();
    expect(exportIdentityBackup).not.toHaveBeenCalled();
    await act(async () => button("Zapisz kopię").click());
    expect(document.querySelector('[role="dialog"]')?.textContent).toContain("Plik zawiera Twój klucz prywatny");
    expect(exportIdentityBackup).not.toHaveBeenCalled();
    await act(async () => button("Anuluj").click());
    expect(document.querySelector('[role="dialog"]')).toBeNull();
    expect(useSettings.getState().onboarding?.nudgeDismissed).toBe(false);
  });
  it("opens account creation from mobile choices without dismissing the reminder", async () => {
    await onPhone();
    await act(async () => useSettings.getState().setOnboarding({ roleReceived: true }));
    await act(async () => button("Zabezpiecz").click());
    await act(async () => button("Załóż konto Gwar Connect").click());
    expect(document.querySelector('[role="dialog"]')).toBeNull();
    expect(useUi.getState()).toMatchObject({ section: "settings", settingsTab: "account", accountEntry: "create" });
    expect(useSettings.getState().onboarding?.nudgeDismissed).toBe(false);
  });
  it("keeps mobile expiry notices short and opens the full sign-in explanation", async () => {
    await onPhone();
    await act(async () => useAccount.setState({ account: { ...account, expiresAt: Date.now() + 5 * day } }));
    expect(container.querySelector('[data-testid="expiry-banner"] p')?.textContent).toBe("Zaloguj się w ciągu 5 dni");
    await act(async () => button("Zaloguj").click());
    expect(document.querySelector('[type="password"]')).not.toBeNull();
    expect(document.querySelector('[role="dialog"]')?.textContent).toContain("Wpisz hasło do Gwar Connect");
    await act(async () => button("Anuluj").click());
    await act(async () => { vi.setSystemTime(Date.now() + 5 * day); window.dispatchEvent(new Event("focus")); });
    expect(container.querySelector('[data-testid="expiry-banner"] p')?.textContent).toBe("Logowanie wygasło");
    await act(async () => button("Zaloguj").click());
    expect(document.querySelector('[type="password"]')).not.toBeNull();
  });
  it("shows the reminder on role receipt and remembers dismissal", async () => {
    await act(async () => useSession.setState({ me: { uid: "me", session: 1 }, members: { me: { uid: "me", nickname: "Alex", tag: "alex", groups: [1, 2], last_seen: 1 } } }));
    expect(container.querySelector('[data-testid="identity-banner"]')).not.toBeNull();
    expect(container.textContent).toContain("bez kopii stracisz nazwę i role");
    expect(container.textContent).not.toContain("klucz prywatny");
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
  it("offers account creation; signed-in accounts hide the reminder", async () => {
    await act(async () => useSettings.getState().setOnboarding({ roleReceived: true }));
    await act(async () => button("Załóż konto").click());
    expect(useUi.getState()).toMatchObject({ section: "settings", settingsTab: "account", accountEntry: "create" });
    await act(async () => useAccount.setState({ account: { ...account, expiresAt: Date.now() + 90 * day } }));
    expect(container.textContent).toBe("");
  });
  it("shows the private-key warning only in the save confirmation; cancelling keeps the reminder", async () => {
    await act(async () => useSettings.getState().setOnboarding({ roleReceived: true }));
    expect(container.textContent).not.toContain("klucz prywatny");
    await act(async () => button("Zapisz kopię").click());
    const dialog = document.querySelector('[role="dialog"]');
    expect(dialog?.textContent).toContain("Plik zawiera Twój klucz prywatny. Nie udostępniaj go nikomu — kto go ma, może podszyć się pod Ciebie.");
    expect(exportIdentityBackup).not.toHaveBeenCalled();
    await act(async () => button("Anuluj").click());
    expect(document.querySelector('[role="dialog"]')).toBeNull();
    expect(exportIdentityBackup).not.toHaveBeenCalled();
    expect(useSettings.getState().onboarding?.nudgeDismissed).toBe(false);
    expect(container.querySelector('[data-testid="identity-banner"]')).not.toBeNull();
  });
  it("waits for the confirmed backup save before dismissing the reminder", async () => {
    let finish!: () => void;
    vi.mocked(exportIdentityBackup).mockImplementation(() => new Promise<void>(resolve => { finish = () => { useSettings.getState().setOnboarding({ nudgeDismissed: true }); resolve(); }; }));
    await act(async () => useSettings.getState().setOnboarding({ roleReceived: true }));
    await act(async () => button("Zapisz kopię").click());
    await act(async () => button("Zapisz").click());
    expect(exportIdentityBackup).toHaveBeenCalledOnce();
    expect(button("Zapisz").disabled).toBe(true);
    expect(useSettings.getState().onboarding?.nudgeDismissed).toBe(false);
    await act(async () => finish());
    expect(document.querySelector('[role="dialog"]')).toBeNull();
    expect(container.querySelector('[data-testid="identity-banner"]')).toBeNull();
  });
  it("keeps the reminder after a cancelled platform save or failed export", async () => {
    await act(async () => useSettings.getState().setOnboarding({ roleReceived: true }));
    // A cancelled platform save resolves without setting nudgeDismissed.
    vi.mocked(exportIdentityBackup).mockResolvedValueOnce();
    await act(async () => button("Zapisz kopię").click());
    await act(async () => button("Zapisz").click());
    expect(document.querySelector('[role="dialog"]')).toBeNull();
    expect(useSettings.getState().onboarding?.nudgeDismissed).toBe(false);
    expect(container.querySelector('[data-testid="identity-banner"]')).not.toBeNull();
    vi.mocked(exportIdentityBackup).mockRejectedValueOnce(new Error("Disk unavailable"));
    await act(async () => button("Zapisz kopię").click());
    await act(async () => button("Zapisz").click());
    expect(document.querySelector('[role="dialog"]')).not.toBeNull();
    expect(button("Zapisz").disabled).toBe(false);
    expect(useSettings.getState().onboarding?.nudgeDismissed).toBe(false);
    expect(container.querySelector('[data-testid="identity-banner"]')).not.toBeNull();
    expect(useUi.getState().toasts.some(t => t.text === "Nie udało się wyeksportować kopii. Spróbuj ponownie.")).toBe(true);
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
