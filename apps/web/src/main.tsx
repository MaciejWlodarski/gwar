// First: what was kept under the project's old name moves before any store reads it.
import "./rename-migration";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import "./index.css";
import { consumeInviteFromLocation } from "./state/invite-boot";
import { captureLogs } from "./lib/diagnostics";

// Recent warnings and errors, kept in memory for a bug report.
captureLogs();

if (import.meta.env.MODE === "e2e") await import("./e2e");

consumeInviteFromLocation();

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
