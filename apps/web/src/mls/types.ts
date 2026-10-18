import type { Attachment } from "../proto/Attachment";
import type { MessageSignature } from "../proto/MessageSignature";
export const stores = ["openmls", "groups", "epochs", "outbox"] as const;
export type Store = typeof stores[number];
export interface Change { key: number[]; value: number[] | null }
export type Changes = Record<Store, Change[]>;
export const emptyChanges = (): Changes => ({ openmls: [], groups: [], epochs: [], outbox: [] });
export interface WasmAgent {
  execute(request: unknown): unknown;
  /** Voice agents only: queries that read the parent channel agent. */
  execute_with_parent?(request: unknown, parent: WasmAgent): unknown;
  take_changes(): Changes;
  free(): void;
  /** The device's owner pin and manager checkpoints (docs/e2e-build.md, section 6, 6b). */
  check_owner?(server: string, genesis: string, statements: string[], signatures: string[], inviteOwner?: string): OwnerCheck;
  accept_generation?(server: string, genesis: string, statements: string[], signatures: string[]): void;
  accept_managers?(server: string, anchor: Uint8Array, list: Uint8Array, now: number, revocations: Revocation[]): ManagersCheck;
  owner_view?(server: string): OwnerView | null;
  sign_manager_list?(statement: string): Uint8Array;
  next_anchor?(server: string, confirmers: string[], signedAt: number, serverAnchor: Uint8Array | undefined): string;
  /** `managers` undefined renews the server's (verified) or the accepted list. */
  next_list?(server: string, anchor: Uint8Array | undefined, managers: string[] | undefined, signedAt: number, serverAnchor: Uint8Array | undefined, serverList: Uint8Array | undefined): string;
  transfer_statement?(server: string, accountKey: string, newOwner: string, serverAnchor: Uint8Array | undefined, signedAt: number): string;
}
export type OwnerCheck =
  | { kind: "pinned" } | { kind: "unchanged" } | { kind: "extended" } | { kind: "changed" }
  | { kind: "new_generation"; generation: number; owner: string };
/** `renewable`: not current, but this device may renew it (its anchor is current and names it). */
export type ManagersCheck = { kind: "accepted" | "unchanged" | "not_current" | "renewable" | "rollback" };
/** The pinned owner and the managers this device accepted. */
export interface OwnerView {
  generation: number;
  seq: number;
  head: string;
  owner: string;
  confirmers: string[];
  managers: string[];
  list_signer: string | null;
  list_signed_at: number | null;
  /** When the list signer's device certificate expires (null: a local identity, or no list). */
  list_signer_expires_at: number | null;
  anchor_seq: number | null;
  list_seq: number | null;
}
/** An owner reset, shown as a neutral notice: the device follows it only after `accept`. */
export interface OwnerGeneration { generation: number; owner: string; accept: () => Promise<void> }
export interface AgentFactory {
  create(): WasmAgent;
  load(records: Changes): WasmAgent;
}
export interface GroupSummary {
  group: number[];
  conversation: string;
  /** Decimal; 0, then one more per restart. */
  incarnation: string;
  /** The conversation's live pin: its highest accepted incarnation. */
  live: boolean;
  history: boolean;
  /** A voice group's parent channel group, from its validated descriptor. */
  channel_group?: string;
  record: {
    status: "creating" | "active" | "joining" | "removed" | "rejoin" | "superseded";
    checkpoint: { epoch: string; hash: number[] } | null;
    server_pending: boolean;
    welcome_ids?: string[];
    own_commit_at?: number | null;
    /** Voice only: devices whose leaves the last merged commit removed. */
    voice_removed?: string[];
    /** Per uid, the epoch ranges in which it had a leaf, as this device saw them. */
    evidence?: Record<string, { first: string; last: string }[]>;
  };
  members: { leaf: number; device_key: string; uid: string }[];
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
export interface Roster {
  /** The live group; rows of earlier incarnations belong to other groups. */
  group: string;
  /** The conversation was restarted at least once. */
  restarted: boolean;
  history: boolean;
  members: GroupSummary["members"];
  /** The first epoch my account had a leaf in, as this device saw it. */
  since: number | null;
}
export interface HpkePublication { device_key: string; hpke_key: string; created_at: number; signature: string }
/** `assignment` answers the server's routing in channel groups (none in direct messages). */
export interface HistoryJob { publication: HpkePublication | null; kind: "history"; server: string; group: string; recipient: string; chunks: string[] | null; assignment?: string | null }
export interface PackageJob { kind: "packages"; server: string; packages: string[]; last_resort: string; expires_at: number }
/** A channel group joined by external commit still owes `mls.history.request`. */
export interface RequestJob { kind: "request"; server: string; group: string }
export type Job = HistoryJob | PackageJob | RequestJob;
