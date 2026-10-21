/**
 * What a dragged person carries to the channel they are dropped on: the
 * session id and who held it at drag start, so a drop after they left and
 * someone else got the same id moves nobody (the server checks the uid).
 */
export interface ClientDrag {
  id: number;
  uid: string;
}

export const CLIENT_MIME = "application/x-gwar-client";

export function clientDragData(drag: ClientDrag): string {
  return JSON.stringify({ id: drag.id, uid: drag.uid });
}

/** The drag data back, or null for anything that is not a person drag from this app. */
export function readClientDrag(data: string): ClientDrag | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(data);
  } catch {
    return null;
  }
  if (!parsed || typeof parsed !== "object") return null;
  const { id, uid } = parsed as Record<string, unknown>;
  return typeof id === "number" && Number.isSafeInteger(id) && id > 0 && typeof uid === "string" && uid !== "" ? { id, uid } : null;
}
