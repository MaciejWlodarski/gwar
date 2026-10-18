/**
 * The key archive loop of a Connect device (docs/e2e-build.md, section 7, "Client"): it publishes
 * this device's archive key and a pending recovery archive key, takes in the account's archive
 * (devices, recovery keys, revocations, manifests, transfers), imports records it can open,
 * uploads epoch keys under the newest safe generation (creating one when there is none) and
 * sends transfers to recipients that lack a generation. Agent work runs in short engine
 * operations; Connect is reached between them, so messaging never waits for it.
 */
import { ConnectApiError, type ArchiveDevice, type ArchiveStatement } from "../connect/api";
import { recoveryVersion } from "../connect/archive";
import { decodeBase64Url, encodeBase64Url } from "../net/base64url";
import { jobChange, type Engine } from "./engine";
import type { ArchiveJob, ArchiveState, ArchiveUpload } from "./types";

/** The Connect calls the archive uses (`ConnectApi`). */
export interface ArchiveConnect {
  putArchiveDevice(token: string, key: ArchiveStatement): Promise<void>;
  archiveDevices(token: string): Promise<ArchiveDevice[]>;
  putArchiveRecovery(token: string, key: ArchiveStatement): Promise<void>;
  archiveRecovery(token: string): Promise<ArchiveStatement[]>;
  postArchiveManifest(token: string, manifest: string): Promise<void>;
  archiveManifests(token: string, after: number): Promise<{ manifests: { generation: number; manifest: string }[]; next: number | null }>;
  postArchiveRecords(token: string, records: { generation: number; record_id: string; data: string }[]): Promise<void>;
  archiveRecords(token: string, after: number): Promise<{ records: { cursor: number; generation: number; record_id: string; data: string }[]; next: number | null }>;
  postArchiveTransfer(token: string, transfer: string): Promise<void>;
  archiveTransfers(token: string, after: number): Promise<{ transfers: { cursor: number; transfer: string }[]; next: number | null }>;
}

/** This device's Connect session, as its stored record has it now. */
export interface ArchiveSession {
  token: string;
  accountKey: string;
  /** A recovery archive key statement to publish (`ConnectRecord.archiveRecovery`). */
  recovery?: ArchiveStatement;
  /** The recovery archive key after a recovery-code sign-in (`ConnectRecord.archiveRecoverySecret`). */
  recoverySecret?: string;
}

export interface ArchiveOptions {
  api: ArchiveConnect;
  /** Null without a Connect account (a local identity). */
  session: () => Promise<ArchiveSession | null>;
  /** Clears what the archive used from the stored record (only if it is still the same). */
  used: (what: { recovery?: ArchiveStatement; recoverySecret?: string }) => Promise<void>;
  /** "Back up message keys to Gwar Connect": off means no uploads; reading and transfers still work. */
  uploads: () => boolean;
  /** The account's revocations with their signatures (Connect's feed). */
  revocations: () => { account_key: string; device_key: string; revoked_at: number; signature: string }[];
  deviceKey: string;
  now: () => number;
}

const KEY = "@runtime/archive";
/** Records per upload, and pages per pass for each list (Connect's quotas: 1,000 manifests and
 * 10,000 transfers in pages of 100; records in pages of 1,000): a pass ends, the next continues. */
const BATCH = 100;
const MAX_BATCHES = 50;
const MAX_MANIFEST_PAGES = 11;
const MAX_TRANSFER_PAGES = 101;
const MAX_RECORD_PAGES = 50;

const dec = new TextDecoder();
const bytes = (text: string): number[] => [...decodeBase64Url(text)];
const conflict = (error: unknown) => error instanceof ConnectApiError && error.kind === "conflict";

/** The next cursor of a page, which must move forward (a Connect that repeats one gets nowhere). */
function advance(after: number, page: { next: number | null }, items: number, max: number): number | null {
  if (items > max) throw new Error("archive page too long");
  if (page.next !== null && !(Number.isSafeInteger(page.next) && page.next > after)) throw new Error("archive page cursor did not advance");
  return page.next;
}

/** The generation a transfer names (its statement's third line), or null for anything else. */
export function transferGeneration(transfer: Uint8Array): number | null {
  const colon = transfer.indexOf(58);
  if (colon < 1 || colon > 6) return null;
  const length = Number(dec.decode(transfer.subarray(0, colon)));
  if (!Number.isSafeInteger(length) || colon + 1 + length > transfer.length) return null;
  const lines = dec.decode(transfer.subarray(colon + 1, colon + 1 + length)).split("\n");
  const generation = Number(lines[2]);
  return lines[0] === "gwar archive transfer v1" && Number.isSafeInteger(generation) && generation > 0 ? generation : null;
}

export class KeyArchive {
  constructor(private readonly engine: Engine, private readonly o: ArchiveOptions) {}

  private job(): ArchiveJob {
    return (this.engine.jobs.get(KEY) as ArchiveJob | undefined) ?? { kind: "archive", newest: 0, records: 0, transfers: 0, opened: [] };
  }

  /** The account's revocations in Connect's feed now: every operation judges eligibility with them. */
  private revocations(session: ArchiveSession) {
    return this.o.revocations()
      .filter(r => r.account_key === session.accountKey)
      .map(r => ({ device_key: r.device_key, revoked_at: r.revoked_at, signature: r.signature }));
  }

  /** One pass; throws on the first failure (the caller retries later). Returns what the
   * device knows of the archive at its end, or null without a Connect session. */
  async pass(): Promise<ArchiveState | null> {
    const session = await this.o.session();
    if (!session) return null;
    const { api } = this.o;
    const { token } = session;
    if (session.recovery) await this.publishRecovery(token, session.recovery);
    let devices = await api.archiveDevices(token);
    if (!devices.some(d => d.statement.split("\n")[2] === this.o.deviceKey)) {
      const [statement, signature] = await this.engine.run(async () => this.engine.query<[string, string]>({ archive_device_key: { now: this.o.now() } }));
      await api.putArchiveDevice(token, { statement, signature }).catch((error: unknown) => { if (!conflict(error)) throw error; });
      devices = await api.archiveDevices(token);
    }
    let state = await this.sync(token, session, devices);
    if (session.recoverySecret) await this.o.used({ recoverySecret: session.recoverySecret });
    await this.importRecords(token, state);
    if (this.o.uploads()) {
      // Without a safe generation it makes the next one; if another device made that number
      // first, the next sync takes it in and judges it.
      for (let attempt = 0; state.safe === null && attempt < 3; attempt++) {
        const manifest = await this.engine.run(async () => this.engine.query<number[]>({ archive_create_manifest: { now: this.o.now(), revocations: this.revocations(session) } }));
        await api.postArchiveManifest(token, encodeBase64Url(new Uint8Array(manifest))).catch((error: unknown) => { if (!conflict(error)) throw error; });
        state = await this.sync(token, session, devices);
      }
      if (state.safe !== null) await this.upload(token, session);
    }
    await this.transfers(token, session, state);
    return state;
  }

  /** Publishes a recovery archive key the account signed (at sign-up, or after Connect took a new
   * code); a newer version on Connect replaced it. */
  private async publishRecovery(token: string, recovery: ArchiveStatement): Promise<void> {
    const key = { statement: recovery.statement, signature: recovery.signature };
    try { await this.o.api.putArchiveRecovery(token, key); }
    catch (error) {
      if (!conflict(error)) throw error;
      const newest = Math.max(0, ...(await this.o.api.archiveRecovery(token)).map(k => recoveryVersion(k.statement)));
      if (newest < recoveryVersion(key.statement)) throw error;
    }
    await this.o.used({ recovery: key });
  }

  private syncPage(session: ArchiveSession, devices: ArchiveDevice[], recovery: string[][], manifests: number[][], transfers: number[][], job: Partial<ArchiveJob>): Promise<ArchiveState> {
    return this.engine.run(() => this.engine.op<ArchiveState>({
      archive_sync: {
        devices: devices.map(({ statement, signature, certificate: c }) => ({
          statement, signature, certificate: c && { issued_at: c.issued_at, expires_at: c.expires_at, signature: c.signature },
        })),
        recovery, revocations: this.revocations(session), manifests, transfers,
        recovery_secret: session.recoverySecret ? bytes(session.recoverySecret) : null,
        now: this.o.now(),
      },
    }, state => [jobChange({ ...this.job(), ...job, newest: state.newest ?? 0 })]));
  }

  /** Takes in the archive a page at a time: manifests first, then transfers. */
  private async sync(token: string, session: ArchiveSession, devices: ArchiveDevice[]): Promise<ArchiveState> {
    const { api } = this.o;
    const recovery = (await api.archiveRecovery(token)).map(k => [k.statement, k.signature]);
    let state: ArchiveState | undefined;
    for (let after: number | null = this.job().newest, pages = 0; after !== null && pages < MAX_MANIFEST_PAGES; pages++) {
      const page = await api.archiveManifests(token, after);
      const next = advance(after, page, page.manifests.length, 100);
      state = await this.syncPage(session, devices, recovery, page.manifests.map(m => bytes(m.manifest)), [], {});
      after = next;
    }
    for (let after: number | null = this.job().transfers, pages = 0; after !== null && pages < MAX_TRANSFER_PAGES; pages++) {
      const page = await api.archiveTransfers(token, after);
      const next = advance(after, page, page.transfers.length, 100);
      // The cursor stops before a transfer of a generation it has not taken in yet: its manifest
      // may have come after the manifests were read, and the next pass reads both.
      const transfers: number[][] = [];
      let cursor = after, waiting = false;
      for (const t of page.transfers) {
        const transfer = decodeBase64Url(t.transfer);
        if ((transferGeneration(transfer) ?? 0) > (state?.newest ?? 0)) { waiting = true; break; }
        transfers.push([...transfer]);
        cursor = Math.max(cursor, t.cursor);
      }
      state = await this.syncPage(session, devices, recovery, [], transfers, { transfers: cursor });
      if (waiting) break;
      after = next;
    }
    return state!;
  }

  /** Records of the generations it opened; a newly opened generation reads them from the start. */
  private async importRecords(token: string, state: ArchiveState): Promise<void> {
    const job = this.job();
    const fresh = state.opened.some(g => !job.opened.includes(g));
    let after: number | null = fresh ? 0 : job.records;
    for (let pages = 0; after !== null && pages < MAX_RECORD_PAGES; pages++) {
      const page = await this.o.api.archiveRecords(token, after);
      const next = advance(after, page, page.records.length, 1000);
      const records = page.records.filter(r => state.opened.includes(r.generation)).map(r => [r.generation, r.record_id, r.data]);
      const cursor = Math.max(after, ...page.records.map(r => r.cursor));
      await this.engine.run(() => this.engine.op<number>({ archive_import: { records } },
        () => [jobChange({ ...this.job(), records: cursor, opened: state.opened })]));
      after = next;
    }
  }

  private async upload(token: string, session: ArchiveSession): Promise<void> {
    for (let batch = 0; batch < MAX_BATCHES; batch++) {
      const upload = await this.engine.run(async () => this.engine.query<ArchiveUpload | null>({ archive_seal: { limit: BATCH, now: this.o.now(), revocations: this.revocations(session) } }));
      if (!upload?.records.length) return;
      await this.o.api.postArchiveRecords(token, upload.records.map(([record_id, data]) => ({ generation: upload.generation, record_id, data })));
      await this.engine.run(() => this.engine.op({ archive_uploaded: { epochs: upload.epochs } }));
    }
  }

  private async transfers(token: string, session: ArchiveSession, state: ArchiveState): Promise<void> {
    for (const recipient of state.waiting) {
      // One at a time, each judged with the feed as it is then and marked sent once Connect has it.
      for (let sent = 0; sent < MAX_TRANSFER_PAGES * 100; sent++) {
        // A recipient no longer current (revoked or expired since the sync) gets nothing.
        const [next] = await this.engine.run(async () => this.engine.query<[string, number[]][]>({
          archive_transfers_for: { recipient, now: this.o.now(), revocations: this.revocations(session), limit: 1 },
        })).catch(() => []);
        if (!next) break;
        await this.o.api.postArchiveTransfer(token, encodeBase64Url(new Uint8Array(next[1])));
        await this.engine.run(() => this.engine.op({ archive_transfers_sent: { tags: [next[0]] } }));
      }
    }
  }
}
