import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import "./index.css";
import { consumeInviteFromLocation } from "./state/invite-boot";
import { captureLogs } from "./lib/diagnostics";
import { isDesktop, tauri } from "./platform";
import { storedDataProblem } from "./lib/stored-data-check";
import { StoredDataError } from "./ui/StoredDataError";

// Recent warnings and errors, kept in memory for a bug report.
captureLogs();

if (import.meta.env.MODE === "e2e") await import("./e2e");

consumeInviteFromLocation();

// The screen-capture check exists only in bundles built with `GWAR_CAPTURE_CHECK=1`; a debug
// desktop app started with it then shows the check instead of the app.
const captureCheck = __CAPTURE_CHECK__ && isDesktop() && (await (await tauri()).invoke<boolean>("debug_capture_check").catch(() => false));
const { CaptureCheck } = __CAPTURE_CHECK__ && captureCheck ? await import("./platform/capture-check") : { CaptureCheck: null };

// Data saved by a newer version (or localStorage that could not be migrated) stops the app before
// anything reads or changes it.
const problem = await storedDataProblem();
if (problem !== undefined) console.error("Saved data unusable:", problem);

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    {problem !== undefined ? <StoredDataError problem={problem} /> : CaptureCheck ? <CaptureCheck /> : <App />}
  </StrictMode>,
);
