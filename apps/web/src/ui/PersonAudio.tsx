import { useT } from "../i18n";
import { useSettings } from "../state/settings";
import { Button, Slider, Switch } from "./kit";

/** Device-local playback controls shared by the menu and the touch-accessible profile. */
export function PersonAudio({ uid }: { uid: string }) {
  const t = useT();
  const volume = useSettings(s => s.userVolumes[uid] ?? 1);
  const muted = useSettings(s => s.userMuted[uid] === true);
  const setVolume = useSettings(s => s.setUserVolume);
  const setMuted = useSettings(s => s.setUserMuted);
  return <div className="flex flex-col gap-3" onPointerDown={e => e.stopPropagation()} onKeyDown={e => e.stopPropagation()}>
    <Switch checked={muted || volume === 0} onCheckedChange={value => { setMuted(uid, value); if (!value && volume === 0) setVolume(uid, 1); }} label={t("person.mute")} />
    <div className="flex items-center justify-between gap-2 text-xs text-muted">
      <span>{t("person.volume")}</span>
      <Button size="sm" variant="ghost" className="h-7 px-1 tabular-nums" title={t("tree.volumeReset")} onClick={() => { setMuted(uid, false); setVolume(uid, 1); }}>{Math.round(volume * 100)}%</Button>
    </div>
    <Slider value={Math.round(volume * 100)} min={0} max={200} step={5} onChange={value => { setVolume(uid, value / 100); setMuted(uid, false); }} label={t("person.volume")} />
  </div>;
}
