/**
 * Before the first TeamSpeak connection on a device: an empty identity list would silently make a
 * new identity, so first offer the one the official TeamSpeak client has on this computer (once per
 * device). Whatever goes wrong here is logged and ignored; connecting never waits on it for long.
 */
import { MIN_LEVEL, tsBridge, type TsBridge, type TsFound } from "../connect/teamspeak";
import { DEFAULT_NAME } from "../connect/ts-list";
import { accountActions } from "./account";
import { useSettings } from "./settings";
import { useUi } from "./stores";

/** Asks which of `found` to use; null means "create a new one instead". */
function askWhich(found: TsFound[]): Promise<TsFound[] | null> {
  return new Promise((resolve) => useUi.getState().openDialog({ kind: "tsFirstRun", found, resolve }));
}

async function offerOfficialIdentities(ts: TsBridge): Promise<void> {
  if (useSettings.getState().tsDetectAsked) return;
  const usable = (await ts.detect()).filter((f) => f.level >= MIN_LEVEL);
  if (usable.length === 0) return;
  // Asked once per device, whatever the answer.
  useSettings.getState().setTsDetectAsked(true);
  const chosen = await askWhich(usable);
  if (chosen && chosen.length > 0) await accountActions.addTeamspeak(chosen);
}

/** Makes sure there is an identity to connect with (the desktop makes "Default" itself if this fails). */
export async function prepareTeamspeak(): Promise<void> {
  const ts = tsBridge();
  if (!ts) return;
  try {
    if ((await ts.list("active")).identities.length > 0) return;
    try {
      await offerOfficialIdentities(ts);
    } catch (e) {
      console.warn("could not look for TeamSpeak identities", e);
    }
    if ((await ts.list("active")).identities.length === 0) await accountActions.generateTeamspeak(DEFAULT_NAME);
  } catch (e) {
    console.warn("could not prepare the TeamSpeak identity", e);
  }
}
