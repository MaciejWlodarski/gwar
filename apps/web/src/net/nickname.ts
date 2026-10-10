/** Member nicknames use the same character limit as the protocol. */
export const NICKNAME_MAX_LEN = 32;

export function validNickname(value: unknown): string | undefined {
  if (typeof value !== "string") return undefined;
  const nickname = value.trim();
  return nickname && [...nickname].length <= NICKNAME_MAX_LEN ? nickname : undefined;
}

/** Five independent decimal digits; rejection sampling avoids modulo bias. */
export function defaultNickname(): string {
  let digits = "";
  while (digits.length < 5) {
    const bytes = crypto.getRandomValues(new Uint8Array(8));
    for (const byte of bytes) {
      if (byte < 250) digits += String(byte % 10);
      if (digits.length === 5) break;
    }
  }
  return `gwar-${digits}`;
}

/** A one-time migration source; identity records become authoritative afterwards. */
export function legacyNickname(): string | undefined {
  try {
    const state = JSON.parse(localStorage.getItem("vc.settings") ?? "{}").state;
    const remembered = validNickname(state?.lastNickname);
    if (remembered) return remembered;
    for (const bookmark of state?.bookmarks ?? []) {
      const nickname = validNickname(bookmark?.nickname);
      if (nickname) return nickname;
    }
  } catch {
    // Storage may be unavailable, or the old settings may be malformed.
  }
  return undefined;
}
