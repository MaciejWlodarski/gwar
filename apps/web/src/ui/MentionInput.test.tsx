// @vitest-environment happy-dom
import { act, useRef } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { useSession } from "../state/stores";
import { useMentionAutocomplete } from "./MentionInput";

let container: HTMLDivElement, root: Root;
let mention!: { mentionsFor: (text: string) => string[] };
type Mention = ReturnType<typeof useMentionAutocomplete>;
function Harness({ text, draft, initial, expose }: { text: string; draft: string; initial: string[]; expose: (m: Mention) => void }) {
  const ref = useRef<HTMLTextAreaElement>(null);
  expose(useMentionAutocomplete(text, () => {}, ref, initial, draft));
  return <textarea ref={ref} value={text} readOnly />;
}
const show = (text: string, draft: string, initial: string[] = []) => act(async () => root.render(<Harness text={text} draft={draft} initial={initial} expose={m => { mention = m; }} />));

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  useSession.setState({ members: { alice: { uid: "alice", nickname: "Alice", groups: [], tag: "alice", last_seen: 0 } }, clients: {} });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); vi.unstubAllGlobals(); });

it("keeps each draft's picked mentions when the composer switches drafts", async () => {
  await show("hi @Alice", "ch:1", ["alice"]);
  expect(mention.mentionsFor("hi @Alice")).toEqual(["alice"]);
  await show("", "ch:2");
  expect(mention.mentionsFor("@Alice")).toEqual([]);
  await show("hi @Alice", "ch:1");
  expect(mention.mentionsFor("hi @Alice")).toEqual(["alice"]);
});
