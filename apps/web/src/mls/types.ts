import type { Attachment } from "../proto/Attachment";
import type { MessageSignature } from "../proto/MessageSignature";
export const stores = ["openmls", "groups", "epochs", "outbox"] as const;
export type Store = typeof stores[number];
export interface Change { key: number[]; value: number[] | null }
export type Changes = Record<Store, Change[]>;
export const emptyChanges = (): Changes => ({ openmls: [], groups: [], epochs: [], outbox: [] });
export interface WasmAgent {
  execute(request: unknown): unknown;
  take_changes(): Changes;
  free(): void;
}
export interface AgentFactory {
  create(): WasmAgent;
  load(records: Changes): WasmAgent;
}
export interface GroupSummary {
  group: number[];
  conversation: string;
  history: boolean;
  record: {
    status: "creating" | "active" | "joining" | "removed" | "rejoin";
    checkpoint: { epoch: string; hash: number[] } | null;
    server_pending: boolean;
    welcome_ids?: string[];
    own_commit_at?: number | null;
  };
  members: { device_key: string; uid: string }[];
}
export interface Gate {
  status: GroupSummary["record"]["status"];
  blocked: number[];
  server_pending: boolean;
  rejoin: boolean;
  pending_commit: boolean;
}
export interface CommitOutbox {
  group: number[]; epoch: string; hash: number[]; commit: number[];
  welcome: number[] | null; group_info: number[];
}
export type Resolution = "accepted" | "retry" | "rebuild" | "rejoin";
export interface Revocation { account_key: string; device_key: string; revoked_at: number }
export interface Plaintext { text: string; mentions: string[]; attachments: Attachment[]; signature: MessageSignature }
export interface SealedFields {
  conversation: string; group: string; epoch: number | string;
  message_id: string; revision: number; ciphertext: string;
}
export type OpenResult = { ok: Plaintext } | { reason: "no_key" | "cannot_open" };
export type ConversationStatus = "ready" | "securing" | "creating";
export interface HpkePublication { device_key: string; hpke_key: string; created_at: number; signature: string }
/** `assignment` answers the server's routing in channel groups (none in direct messages). */
export interface HistoryJob { publication: HpkePublication | null; kind: "history"; server: string; group: string; recipient: string; chunks: string[] | null; assignment?: string | null }
export interface PackageJob { kind: "packages"; server: string; packages: string[]; last_resort: string; expires_at: number }
export type Job = HistoryJob | PackageJob;
