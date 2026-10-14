// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { beforeEach, afterEach, describe, expect, it, vi } from "vitest";
import { useSettings } from "../state/settings";
import { useSession } from "../state/stores";
import type { ChatMessage } from "../proto/ChatMessage";
import type { Row } from "../lib/chat";
import { DELETION_TEXT, verifyMessage, type MessageVerification } from "../net/message-signature";
import { MessageRow } from "./Chat";
import { TooltipProvider } from "./kit";

vi.mock("./useVerifiedHandle", () => ({ useVerifiedHandle: () => null }));
vi.mock("../state/controller", () => ({ controller: {}, UploadError: class extends Error {} }));
const valid: MessageVerification = { status: "verified", continuity: "proven", authorization: [] };
const msg: ChatMessage = { id: 1, target: { channel: 7 }, author: 1, author_uid: "author", author_name: "Alice", text: "hello @Bob", mentions: ["bob"], sent_at: 1700000000000 };

describe("quiet localized message verification", () => {
  let container: HTMLDivElement, root: Root;
  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    useSettings.setState({ language: "en" });
    useSession.setState({ members: { bob: { uid: "bob", nickname: "Bob", groups: [], tag: "bob", last_seen: 0 } } });
    container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  });
  afterEach(async () => {
    await act(async () => root.unmount()); container.remove(); vi.unstubAllGlobals();
  });
  const show = async (verification: MessageVerification | undefined, options: { first?: boolean; text?: string; removed?: boolean; cannotOpen?: "no_key" | "cannot_open"; canEdit?: boolean; canDelete?: boolean } = {}) => {
    const row: Extract<Row, { type: "msg" }> = { type: "msg", key: "m1", first: options.first ?? true,
      item: { kind: "msg", key: "m1", at: msg.sent_at, msg: { ...msg, text: options.text ?? msg.text }, removed: options.removed, cannotOpen: options.cannotOpen } };
    await act(async () => root.render(<TooltipProvider><MessageRow row={row} mine={false} meUid="bob" lang="en" compact={false} canEdit={options.canEdit ?? false} canDelete={options.canDelete ?? false} editing={false} verification={verification} /></TooltipProvider>));
  };
  for (const first of [true, false]) {
    it(`adds no marker or verification text to a fully verified ${first ? "header" : "grouped line"}`, async () => {
      await show(valid, { first });
      expect(container.querySelector("[data-testid^=message-verification]")).toBeNull();
      expect(container.textContent).not.toContain("Signature valid");
      expect(container.textContent).not.toContain("#");
      expect(container.querySelector("[data-mention=bob]")?.textContent).toBe("@Bob");
    });
    it(`uses only a subtle icon for historical ${first ? "headers" : "grouped lines"}`, async () => {
      await show({ ...valid, continuity: "unproven", authorization: ["certificate-expired"] }, { first });
      const icon = container.querySelector('[data-testid="message-verification-qualified"]')!;
      expect(icon.className).toContain("text-subtle"); expect(icon.textContent).toBe("");
      expect(icon.getAttribute("aria-label")).toContain("certificate expired; signing time unproven");
      expect(icon.getAttribute("aria-label")).toContain("no record of earlier versions");
      expect(container.querySelector("p")).toBeNull();
    });
  }
  for (const [language, visible, explanation] of [["en", "Not verified", "Conflicting message revisions"], ["pl", "Niezweryfikowana", "Sprzeczne wersje wiadomości"]] as const) {
    it(`shows the red invalid state and no highlighted mentions in ${language}`, async () => {
      useSettings.setState({ language });
      await show({ ...valid, status: "invalid", continuity: "fork" });
      const icon = container.querySelector('[data-testid="message-verification-invalid"]')!;
      expect(icon.className).toContain("text-danger"); expect(icon.textContent).toBe(visible);
      expect(icon.getAttribute("aria-label")).toContain(explanation);
      expect(container.querySelector("[data-mention]")).toBeNull();
      expect(container.querySelector("[data-message]")?.getAttribute("data-message-verification")).toBe("invalid");
    });
  }
  it("renders migrated unsigned Gwar history with its text and warning", async () => {
    const history = JSON.parse(JSON.stringify({ messages: [msg] })) as { messages: ChatMessage[] };
    const verification = await verifyMessage(history.messages[0]!, { serverKey: "unused", target: "channel:7" });
    expect(verification.status).toBe("unsigned");
    await show(verification);
    expect(container.textContent).toContain("hello @Bob");
    expect(container.querySelector('[data-testid="message-verification-unsigned"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="message-verification-invalid"]')).toBeNull();
  });
  it("warns about unsigned Gwar messages with an icon and tooltip only", async () => {
    await show({ ...valid, status: "unsigned" });
    const icon = container.querySelector('[data-testid="message-verification-unsigned"]')!;
    expect(icon.className).toContain("text-warn"); expect(icon.textContent).toBe("");
    expect(icon.getAttribute("aria-label")).toContain("Unsigned: this server may have forged or altered this message");
  });
  it("reuses the TeamSpeak badge at the author and discloses its unsigned origin", async () => {
    await show({ ...valid, status: "unsigned-ts" });
    const badge = container.querySelector('[data-testid="message-verification-teamspeak"]')!;
    expect(badge.textContent).toBe("TS"); expect(badge.getAttribute("aria-label")).toContain("TeamSpeak messages are unsigned");
    expect(container.querySelector('[aria-label^="Gwar Connect account"]')).toBeNull();
  });
  for (const continuity of ["proven", "unproven"] as const) it(`hides a verified author deletion (continuity ${continuity})`, async () => {
    await show({ ...valid, continuity }, { text: DELETION_TEXT });
    expect(container.querySelector("[data-message]")).toBeNull(); expect(container.textContent).toBe("");
  });
  it("keeps a qualified author deletion as an inspectable tombstone", async () => {
    await show({ ...valid, authorization: ["device-revoked"] }, { text: DELETION_TEXT });
    expect(container.querySelector('[data-testid="message-author-deleted"]')?.textContent).toBe("Deleted by the author");
    expect(container.querySelector('[data-testid="message-verification-qualified"]')?.getAttribute("aria-label")).toContain("device revoked");
    expect(container.textContent).not.toContain("Unverified deletion request");
    await show({ ...valid, authorization: ["revocations-stale"] }, { text: DELETION_TEXT });
    expect(container.querySelector('[data-testid="message-author-deleted"]')).not.toBeNull();
  });
  it("shows a message as pending until its signature is checked", async () => {
    await show(undefined);
    expect(container.querySelector('[data-testid="message-verification-pending"]')?.getAttribute("aria-label")).toBe("Checking the signature…");
    expect(container.querySelector("[data-message]")?.getAttribute("data-message-verification")).toBe("pending");
    expect(container.querySelector("[data-mention]")).toBeNull();
    expect(container.querySelector(".text-muted")?.textContent).toContain("hello");
  });
  it("keeps a forged deletion visible as invalid", async () => {
    await show({ ...valid, status: "invalid" }, { text: DELETION_TEXT });
    expect(container.querySelector('[data-testid="message-verification-invalid"]')).not.toBeNull();
    expect(container.textContent).toContain("Unverified deletion request");
    expect(container.querySelector('[data-testid="message-server-removed"]')).toBeNull();
  });
  it("offers only removal for a row this device cannot open", async () => {
    const button = (name: string) => container.querySelector(`button[aria-label="${name}"]`);
    await show(undefined, { text: "", cannotOpen: "cannot_open", canEdit: true, canDelete: true });
    expect(button("Delete message")).not.toBeNull();
    expect(button("Edit message")).toBeNull();
    await show(undefined, { text: "", cannotOpen: "no_key", canEdit: true });
    expect(button("Delete message")).toBeNull(); expect(button("Edit message")).toBeNull();
  });
  it("keeps server moderation visible with a localized placeholder", async () => {
    await show(valid, { removed: true });
    expect(container.querySelector('[data-testid="message-server-removed"]')?.textContent).toBe("Message removed by the server");
    useSettings.setState({ language: "pl" }); await show(valid, { removed: true });
    expect(container.textContent).toContain("Wiadomość usunięta przez serwer"); expect(container.textContent).not.toContain("hello");
  });
});
