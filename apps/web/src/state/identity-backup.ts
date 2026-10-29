import { controller } from "./controller";
import { useSettings } from "./settings";

/** Shared by the reminder and Settings, so either successful export ends the reminder. */
export async function exportIdentityBackup(): Promise<void> {
  const identity = await controller.getIdentity();
  const blob = new Blob([JSON.stringify(identity.exportBackup(), null, 2)], { type: "application/json" });
  const url = URL.createObjectURL(blob);
  try {
    const link = document.createElement("a");
    link.href = url;
    link.download = "gwar-identity-backup.json";
    link.click();
    useSettings.getState().setOnboarding({ nudgeDismissed: true });
  } finally {
    URL.revokeObjectURL(url);
  }
}
