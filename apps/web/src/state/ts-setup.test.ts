import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { TsBridge, TsFound, TsListInfo } from "../connect/teamspeak";

// The first TeamSpeak connection on a device: the desktop is faked, the dialog is answered by the test.
const mocks = vi.hoisted(() => ({
  bridge: null as unknown,
  add: vi.fn(),
  generate: vi.fn(),
}));
vi.mock("../connect/teamspeak", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../connect/teamspeak")>()),
  tsBridge: () => mocks.bridge,
}));
vi.mock("./account", () => ({ accountActions: { addTeamspeak: mocks.add, generateTeamspeak: mocks.generate } }));

let prepareTeamspeak: typeof import("./ts-setup").prepareTeamspeak;
let useSettings: typeof import("./settings").useSettings;
let useUi: typeof import("./stores").useUi;

const found = (uid: string, level = 8, selected = false): TsFound => ({ source: "TeamSpeak 3", name: `n-${uid}`, uid, level, identity: `1V${uid}`, selected });

function desktop(over: { identities?: number; detect?: () => Promise<TsFound[]> } = {}) {
  const calls = { detect: 0 };
  const info = (n: number): TsListInfo => ({
    source: "device",
    default: n ? "x" : null,
    identities: Array.from({ length: n }, (_, i) => ({ uid: `x${i}`, name: "x", identity: "1Vx", level: 8 })),
  });
  const bridge: Partial<TsBridge> = {
    list: async () => info(over.identities ?? 0),
    detect: async () => {
      calls.detect++;
      return (over.detect ?? (async () => []))();
    },
  };
  mocks.bridge = bridge;
  return calls;
}

/** Waits for the first-run dialog and answers it. */
async function answer(chosen: (f: TsFound[]) => TsFound[] | null) {
  await vi.waitFor(() => expect(useUi.getState().dialog.kind).toBe("tsFirstRun"));
  const dialog = useUi.getState().dialog;
  if (dialog.kind !== "tsFirstRun") throw new Error("no dialog");
  dialog.resolve(chosen(dialog.found));
  useUi.getState().closeDialog();
}

beforeAll(async () => {
  vi.stubGlobal("localStorage", { getItem: () => null, setItem: () => {}, removeItem: () => {} });
  vi.stubGlobal("window", { addEventListener: vi.fn(), removeEventListener: vi.fn(), location: { protocol: "http:", hostname: "localhost", port: "5173" } });
  ({ useSettings } = await import("./settings"));
  // No real storage in this environment: keep the settings in memory only.
  useSettings.persist.setOptions({ storage: { getItem: () => null, setItem: () => {}, removeItem: () => {} } });
  ({ useUi } = await import("./stores"));
  ({ prepareTeamspeak } = await import("./ts-setup"));
});

beforeEach(() => {
  mocks.add.mockReset();
  mocks.generate.mockReset();
  useSettings.setState({ tsDetectAsked: false });
  useUi.getState().closeDialog();
});

describe("prepareTeamspeak", () => {
  it("does nothing in the browser and when an identity exists; never looks on the computer then", async () => {
    mocks.bridge = null;
    await prepareTeamspeak();
    const calls = desktop({ identities: 1 });
    await prepareTeamspeak();
    expect(calls.detect).toBe(0);
    expect(mocks.add).not.toHaveBeenCalled();
    expect(mocks.generate).not.toHaveBeenCalled();
    expect(useSettings.getState().tsDetectAsked).toBe(false);
  });

  it("with nothing found, makes a Default identity and does not ask or remember anything", async () => {
    const calls = desktop();
    await prepareTeamspeak();
    expect(calls.detect).toBe(1);
    expect(mocks.generate).toHaveBeenCalledWith("Default");
    expect(useUi.getState().dialog.kind).toBe("none");
    expect(useSettings.getState().tsDetectAsked).toBe(false);
  });

  it("identities below the minimum level are not worth asking about", async () => {
    desktop({ detect: async () => [found("weak", 3)] });
    await prepareTeamspeak();
    expect(useUi.getState().dialog.kind).toBe("none");
    expect(mocks.generate).toHaveBeenCalled();
  });

  it("asks once, adds what was chosen and then does not generate", async () => {
    desktop({ detect: async () => [found("a", 8, true), found("weak", 2), found("b")] });
    const done = prepareTeamspeak();
    await answer((f) => {
      expect(f.map((x) => x.uid)).toEqual(["a", "b"]);
      return [f[0]!];
    });
    await done;
    expect(mocks.add).toHaveBeenCalledWith([expect.objectContaining({ uid: "a" })]);
    expect(useSettings.getState().tsDetectAsked).toBe(true);
    // The fake still reports an empty list, so the fallback runs; a real desktop has the new entry by now.
    expect(mocks.generate).toHaveBeenCalledTimes(1);
  });

  it("choosing a new one makes Default, and the question is not asked again", async () => {
    const calls = desktop({ detect: async () => [found("a")] });
    const done = prepareTeamspeak();
    await answer(() => null);
    await done;
    expect(mocks.add).not.toHaveBeenCalled();
    expect(mocks.generate).toHaveBeenCalledWith("Default");
    expect(useSettings.getState().tsDetectAsked).toBe(true);

    await prepareTeamspeak();
    expect(calls.detect).toBe(1);
    expect(useUi.getState().dialog.kind).toBe("none");
  });

  it("closing the question any other way counts as no", async () => {
    desktop({ detect: async () => [found("a")] });
    const done = prepareTeamspeak();
    await vi.waitFor(() => expect(useUi.getState().dialog.kind).toBe("tsFirstRun"));
    useUi.getState().openDialog({ kind: "invites" });
    await done;
    expect(mocks.add).not.toHaveBeenCalled();
    expect(mocks.generate).toHaveBeenCalled();
  });

  it("a failing search or a failing add never throws out of it", async () => {
    desktop({
      detect: async () => {
        throw new Error("boom");
      },
    });
    await expect(prepareTeamspeak()).resolves.toBeUndefined();
    expect(mocks.generate).toHaveBeenCalled();

    mocks.generate.mockRejectedValue(new Error("disk full"));
    desktop({ detect: async () => [found("a")] });
    const done = prepareTeamspeak();
    await answer((f) => f);
    mocks.add.mockRejectedValue(new Error("vault down"));
    await expect(done).resolves.toBeUndefined();
  });
});
