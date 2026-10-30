import { saveBlob } from "../platform/download";
import { controller } from "./controller";
import { useSettings } from "./settings";

/** Shared by the reminder and Settings, so either successful export ends the reminder. */
export async function exportIdentityBackup(): Promise<void> {
  const identity = await controller.getIdentity();
  const blob = new Blob([JSON.stringify(identity.exportBackup(), null, 2)], { type: "application/json" });
  if (await saveBlob(blob, "gwar-identity-backup.json")) {
    useSettings.getState().setOnboarding({ nudgeDismissed: true });
  }
}
