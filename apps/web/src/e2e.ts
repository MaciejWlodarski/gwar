/** Test-only module access for the built-app browser suite; excluded from release builds. */
const modules: Record<string, () => Promise<unknown>> = {
  "/src/connect/api.ts": () => import("./connect/api.ts"),
  "/src/connect/service-key.ts": () => import("./connect/service-key.ts"),
  "/src/connect/vault.ts": () => import("./connect/vault.ts"),
  "/src/lib/chat.ts": () => import("./lib/chat.ts"),
  "/src/net/handshake.ts": () => import("./net/handshake.ts"),
  "/src/net/identity.ts": () => import("./net/identity.ts"),
  "/src/net/kv.ts": () => import("./net/kv.ts"),
  "/src/net/message-signature.ts": () => import("./net/message-signature.ts"),
  "/src/state/controller.ts": () => import("./state/controller.ts"),
  "/src/state/reducer.ts": () => import("./state/reducer.ts"),
  "/src/state/screen.ts": () => import("./state/screen.ts"),
  "/src/state/settings.ts": () => import("./state/settings.ts"),
  "/src/platform/ptt.ts": () => import("./platform/ptt.ts"),
  "/src/state/stores.ts": () => import("./state/stores.ts"),
  "/src/voice/denoise/index.ts": () => import("./voice/denoise/index.ts"),
  "/src/voice/native-engine.ts": () => import("./voice/native-engine.ts"),
  "/src/voice/input-gate.ts": () => import("./voice/input-gate.ts"),
};
Object.defineProperty(window, "__gwarE2eImport", { value: (name: string) => {
  const load = modules[name];
  if (!load) throw new Error("Unknown e2e module");
  return load();
} });
export {};
