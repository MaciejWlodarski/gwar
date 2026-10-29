// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import { useSettings } from "../state/settings";
import { Connecting } from "./Connecting";
import { Sidebar } from "./Sidebar";
import { useConnectUi, useSession } from "../state/stores";

const disconnect = vi.hoisted(() => vi.fn(async () => {}));
vi.mock("../state/controller", () => ({ controller: { disconnect } }));

it("names the saved server, announces connecting in Polish and lets the person cancel", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  useSettings.setState({ language: "pl", resumeConnection: { kind: "gwar", address: "local.example", name: "Nasza ekipa" } });
  const host = document.createElement("div"); document.body.appendChild(host);
  const root = createRoot(host);
  try {
    await act(async () => root.render(<Connecting />));
    expect(host.querySelector('[role="status"]')?.textContent).toBe("Łączenie…Nasza ekipa");
    expect(host.querySelector("form")).toBeNull();
    await act(async () => host.querySelector("button")!.click());
    expect(disconnect).toHaveBeenCalledOnce();
  } finally {
    await act(async () => root.unmount()); host.remove(); vi.unstubAllGlobals();
  }
});

it("keeps voice and empty-channel notices out of the connecting server shell", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  useSettings.setState({ language: "en", resumeConnection: { kind: "gwar", address: "local.example", name: "Friends" } });
  useSession.setState({ phase: "connecting", address: "local.example", server: null, me: null });
  const host = document.createElement("div"); document.body.appendChild(host);
  const root = createRoot(host);
  try {
    await act(async () => root.render(<Sidebar connecting />));
    expect(host.textContent).toContain("Friends");
    expect(host.textContent).not.toContain("No channels");
    expect(host.textContent).not.toContain("Connecting voice");
  } finally {
    await act(async () => root.unmount()); host.remove(); vi.unstubAllGlobals();
  }
});

it("keeps Connecting and Cancel while explaining an offline retry", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  useSettings.setState({ language: "pl" }); useConnectUi.setState({ startupRetry: true });
  const host = document.createElement("div"); document.body.appendChild(host); const root = createRoot(host);
  try {
    await act(async () => root.render(<Connecting />));
    expect(host.textContent).toContain("Brak połączenia. Próbujemy ponownie…");
    expect(host.querySelector("button")?.textContent).toBe("Anuluj");
  } finally {
    await act(async () => root.unmount()); host.remove(); useConnectUi.setState({ startupRetry: false }); vi.unstubAllGlobals();
  }
});
