import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import "./index.css";
import { consumeInviteFromLocation } from "./state/invite-boot";

if (import.meta.env.MODE === "e2e") await import("./e2e");

consumeInviteFromLocation();

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
