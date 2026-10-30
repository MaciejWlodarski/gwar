import type { ChatMessage } from "../proto/ChatMessage";
import type { SessionState } from "../state/types";
import { myChannelId } from "../state/reducer";
import { CONTROL_CUES as controls, type SoundCue, type SoundSettings } from "./catalog";

/** One dispatcher per controller. No delayed queue: suppressed bursts never play later. */
export class SoundDispatcher {
  private last = new Map<string, number>();
  private channel: number | null = null;
  private recoveryChannel: number | null = null;
  private messages = new Set<string>();

  constructor(
    private readonly settings: () => SoundSettings,
    private readonly deafened: () => boolean,
    private readonly play: (cue: SoundCue, volume: number) => void,
    private readonly now = () => performance.now(),
  ) {}

  cue(cue: SoundCue, preview = false): void {
    const p = this.settings();
    if (!p.enabled || !p.events[cue] || p.volume <= 0 || (this.deafened() && !controls.has(cue))) return;
    // A busy room gets one activity cue per 750 ms; control and connection feedback
    // have separate budgets so a join burst cannot hide a mute or connection loss.
    const group = preview ? "preview" : controls.has(cue) ? "control" : cue === "connectionLost" || cue === "reconnected" ? cue
      : cue === "selfJoin" || cue === "selfLeave" ? "self" : cue === "shareStopped" ? cue : "activity";
    const gap = group === "activity" ? 750 : group === "control" ? 100 : 300;
    const now = this.now();
    if (now - (this.last.get(group) ?? -Infinity) < gap) return;
    this.last.set(group, now);
    this.play(cue, p.volume);
  }

  connected(channel: number | null): void {
    if (channel === null) return;
    if (this.recoveryChannel === channel) this.cue("reconnected");
    else if (this.channel !== channel) this.cue("selfJoin");
    this.channel = channel;
    this.recoveryChannel = null;
  }

  lost(defer = false): boolean {
    if (this.channel === null || this.recoveryChannel !== null) return false;
    this.recoveryChannel = this.channel;
    if (!defer) this.cue("connectionLost");
    return true;
  }

  left(): void {
    if (this.channel !== null) this.cue("selfLeave");
    this.reset();
  }

  /** A new channel is a new participation, even if the old transport was recovering. */
  moving(channel: number): void { if (this.channel !== channel) this.retire(); }

  /** Retire voice intent without claiming either a transport loss or a deliberate departure. */
  retire(channel = this.channel): void {
    if (channel !== this.channel) return;
    this.channel = null;
    this.recoveryChannel = null;
  }

  reset(): void { this.retire(); this.last.clear(); this.messages.clear(); }

  get awaitingRecovery(): boolean { return this.recoveryChannel !== null; }

  /** Welcome/history snapshots never sound like a burst of people joining. */
  session(next: SessionState, before: SessionState): void {
    const channel = myChannelId(next);
    if (channel === null || channel !== myChannelId(before) || next.epoch !== before.epoch || next.phase !== "online" || before.phase !== "online") return;
    const peers = (s: SessionState) => new Set(Object.values(s.clients).filter(c => c.id !== s.me?.session && c.channel === channel).map(c => c.id));
    const old = peers(before), current = peers(next);
    if ([...current].some(id => !old.has(id))) this.cue("peerJoin");
    if ([...old].some(id => !current.has(id))) this.cue("peerLeave");
    if (Object.values(next.shares).some(s => s.channel === channel && s.client !== next.me?.session && !before.shares[s.share])) this.cue("shareStarted");
  }

  /** Called only for verified live messages, after the existing authenticity checks. */
  message(msg: ChatMessage, s: SessionState): void {
    if (!s.me || msg.author_uid === s.me.uid) return;
    const key = `${s.epoch}:${JSON.stringify(msg.target)}:${msg.author_uid}:${msg.id}`;
    if (this.messages.has(key)) return;
    this.messages.add(key);
    if (this.messages.size > 256) this.messages.delete(this.messages.values().next().value!);
    const dm = typeof msg.target === "object" && "client" in msg.target;
    const shown = dm ? s.activeThread === `dm:${msg.author_uid}` : msg.target === "server" ? s.activeThread === "server"
      : "channel" in msg.target && s.activeThread === "channel" && s.viewChannel === msg.target.channel;
    if (s.focused && shown) return;
    if (dm) this.cue("directMessage");
    else if (msg.mentions?.includes(s.me.uid)) this.cue("mention");
  }
}
