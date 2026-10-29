// @vitest-environment happy-dom
import { beforeEach, describe, expect, it, vi } from "vitest";
import { saveBlob } from "../platform/download";
import { useSettings } from "./settings";
import { exportIdentityBackup } from "./identity-backup";
vi.mock("./controller", () => ({ controller: { getIdentity: async () => ({ exportBackup: () => ({ format: "gwar-identity", version: 1, nickname: "Alex", jwk: { d: "test-private-key" } }) }) } }));
vi.mock("../platform/download", () => ({ saveBlob: vi.fn() }));
beforeEach(() => {
  vi.mocked(saveBlob).mockReset();
  useSettings.setState({ onboarding: { completed: true, firstUsedAt: 1, nudgeDismissed: false, roleReceived: true } });
});
describe("identity backup save result", () => {
  it("uses the platform save flow and dismisses the reminder only after a confirmed save", async () => {
    let finish!: (saved: boolean) => void;
    vi.mocked(saveBlob).mockReturnValue(new Promise(resolve => { finish = resolve; }));
    const exporting = exportIdentityBackup();
    await vi.waitFor(() => expect(saveBlob).toHaveBeenCalledOnce());
    expect(useSettings.getState().onboarding?.nudgeDismissed).toBe(false);
    expect(saveBlob).toHaveBeenCalledWith(expect.any(Blob), "gwar-identity-backup.json");
    finish(true);
    await exporting;
    expect(useSettings.getState().onboarding?.nudgeDismissed).toBe(true);
  });
  it("keeps the reminder when the desktop save dialog is cancelled", async () => {
    vi.mocked(saveBlob).mockResolvedValue(false);
    await exportIdentityBackup();
    expect(useSettings.getState().onboarding?.nudgeDismissed).toBe(false);
  });
  it("keeps the reminder if saving fails", async () => {
    vi.mocked(saveBlob).mockRejectedValue(new Error("Save failed"));
    await expect(exportIdentityBackup()).rejects.toThrow("Save failed");
    expect(useSettings.getState().onboarding?.nudgeDismissed).toBe(false);
  });
});
