import { useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent, type RefObject } from "react";
import { activeMention, completeMention, filterPeople, resolveMentions, updatePickedMentions, picksFromMessage, type PickedMention, type Person } from "../lib/mentions";
import { useSession } from "../state/stores";
import { MentionMenu } from "./MentionMenu";

/** Everyone I could mention: members known to the server, with who is online. */
function usePeople(): Person[] {
  const members = useSession((s) => s.members);
  const clients = useSession((s) => s.clients);
  return useMemo(() => {
    const online = new Set(Object.values(clients).map((c) => c.uid));
    return Object.values(members).map((m) => ({ uid: m.uid, nickname: m.nickname, tag: m.tag, connect: m.connect, online: online.has(m.uid) }));
  }, [members, clients]);
}

/**
 * `@` autocomplete for a textarea: tracks the caret, offers matching people
 * (online first), inserts `@Nickname ` and remembers whom to put in `mentions`.
 */
export function useMentionAutocomplete(value: string, setValue: (v: string) => void, ref: RefObject<HTMLTextAreaElement | null>, initialMentions: readonly string[] = []) {
  const people = usePeople();
  const selfUid = useSession((s) => s.me?.uid);
  const [caret, setCaret] = useState(0);
  const [selection, setSelection] = useState({ query: "", index: 0 });
  const [dismissed, setDismissed] = useState<number | null>(null);
  const picked = useRef<PickedMention[]>(picksFromMessage(value, people, initialMentions));
  const previous = useRef(value);
  const editSelection = useRef<{ start: number; end: number } | undefined>(undefined);
  const currentPicks = () => updatePickedMentions(previous.current, value, picked.current, editSelection.current);
  useLayoutEffect(() => {
    picked.current = updatePickedMentions(previous.current, value, picked.current, editSelection.current);
    previous.current = value;
    editSelection.current = undefined;
  }, [value]);
  // Where the caret goes once the completed text is in the textarea (set before the next paint, so typing is never overtaken).
  const pendingCaret = useRef<number | null>(null);
  useLayoutEffect(() => {
    const el = ref.current;
    if (pendingCaret.current === null || !el) return;
    el.setSelectionRange(pendingCaret.current, pendingCaret.current);
    pendingCaret.current = null;
  });

  const active = activeMention(value, Math.min(caret, value.length));
  const matches = useMemo(
    () => (active && dismissed !== active.start ? filterPeople(people, active.query, selfUid) : []),
    [active?.start, active?.query, dismissed, people, selfUid], // eslint-disable-line react-hooks/exhaustive-deps
  );
  const open = matches.length > 0;
  const index = active && selection.query === active.query ? Math.min(selection.index, matches.length - 1) : 0;

  const sync = (el?: HTMLTextAreaElement | null) => setCaret((el ?? ref.current)?.selectionStart ?? value.length);

  const choose = (p: Person) => {
    if (!active) return;
    const next = completeMention(value, active, Math.min(caret, value.length), p.nickname);
    picked.current = [
      ...updatePickedMentions(value, next.text, currentPicks()),
      { uid: p.uid, nickname: p.nickname, start: active.start, end: active.start + p.nickname.length + 1 },
    ];
    previous.current = next.text;
    editSelection.current = undefined;
    pendingCaret.current = next.caret;
    setValue(next.text);
    setCaret(next.caret);
    setDismissed(null);
    ref.current?.focus();
  };

  /** Returns true if the key was used by the menu. */
  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>): boolean => {
    if (!open || !active || e.nativeEvent.isComposing) return false;
    const move = (to: number) => setSelection({ query: active.query, index: (to + matches.length) % matches.length });
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        move(index + 1);
        return true;
      case "ArrowUp":
        e.preventDefault();
        move(index - 1);
        return true;
      case "Enter":
      case "Tab": {
        if (e.shiftKey && e.key === "Enter") return false;
        e.preventDefault();
        const p = matches[index];
        if (p) choose(p);
        return true;
      }
      case "Escape":
        e.preventDefault();
        e.stopPropagation();
        setDismissed(active.start);
        return true;
    }
    return false;
  };

  const menu = open ? <MentionMenu matches={matches} index={index} onPick={choose} onHover={(i) => setSelection({ query: active?.query ?? "", index: i })} /> : null;

  return {
    menu,
    open,
    onKeyDown,
    beforeInput: () => {
      const el = ref.current;
      editSelection.current = el ? { start: el.selectionStart, end: el.selectionEnd } : undefined;
    },
    sync,
    /** Uids to send for this text. */
    mentionsFor: (text: string) => resolveMentions(text, updatePickedMentions(value, text, currentPicks())),
    reset: () => { picked.current = []; },
  };
}
