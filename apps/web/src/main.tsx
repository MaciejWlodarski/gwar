import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import "./index.css";
import { consumeInviteFromLocation } from "./state/invite-boot";
import { captureLogs } from "./lib/diagnostics";
import { isDesktop, tauri } from "./platform";

// Recent warnings and errors, kept in memory for a bug report.
captureLogs();

if (import.meta.env.MODE === "e2e") await import("./e2e");

consumeInviteFromLocation();

// Debug desktop builds can open the screen-capture check instead (`GWAR_CAPTURE_CHECK=1`).
const captureCheck = isDesktop() && (await (await tauri()).invoke<boolean>("debug_capture_check").catch(() => false));
const { CaptureCheck } = captureCheck ? await import("./platform/capture-check") : { CaptureCheck: null };

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    {CaptureCheck ? <CaptureCheck /> : <App />}
  </StrictMode>,
);
