import { Play } from "lucide-react";
import { useT, type Key } from "../i18n";
import { controller } from "../state/controller";
import { useSettings } from "../state/settings";
import type { SoundCue } from "../sound/catalog";
import { Button, Slider, Switch } from "./kit";

const groups: { title: Key; cues: SoundCue[] }[] = [
  { title: "sounds.voice", cues: ["selfJoin", "selfLeave", "peerJoin", "peerLeave"] },
  { title: "sounds.controls", cues: ["mute", "unmute", "deafen", "undeafen"] },
  { title: "sounds.connection", cues: ["connectionLost", "reconnected"] },
  { title: "sounds.messages", cues: ["mention", "directMessage"] },
  { title: "sounds.screen", cues: ["shareStarted", "shareStopped"] },
];

/** Device-local preferences: changes apply immediately, including previews. */
export function SoundSettings() {
  const t = useT();
  const sounds = useSettings(s => s.sounds);
  const setSounds = useSettings(s => s.setSounds);
  const setEvent = useSettings(s => s.setSoundEvent);
  return (
    <div className="space-y-7">
      <section className="space-y-4">
        <div>
          <Switch checked={sounds.enabled} onCheckedChange={enabled => setSounds({ enabled })} label={t("sounds.enabled")} />
          <p className="text-xs leading-relaxed text-muted">{t("sounds.hint")}</p>
        </div>
        <fieldset disabled={!sounds.enabled} className="space-y-2 disabled:opacity-50">
          <div className="text-sm text-fg">{t("sounds.volume")}</div>
          <div className="flex items-center gap-4">
            <Slider value={Math.round(sounds.volume * 100)} min={0} max={100} step={5} onChange={v => setSounds({ volume: v / 100 })} label={t("sounds.volume")} />
            <output className="w-12 shrink-0 text-right text-sm tabular-nums text-muted">{Math.round(sounds.volume * 100)}%</output>
          </div>
        </fieldset>
        <p className="text-xs leading-relaxed text-muted">{t("sounds.deafenHint")}</p>
      </section>
      <fieldset disabled={!sounds.enabled} className="space-y-6 disabled:opacity-50">
        {groups.map(group => (
          <section key={group.title} aria-label={t(group.title)}>
            <h3 className="mb-2 text-sm font-semibold text-muted">{t(group.title)}</h3>
            <div>
              {group.cues.map(cue => (
                <div key={cue} className="flex min-h-12 items-center gap-3">
                  <div className="min-w-0 flex-1 [&_label]:flex [&_label]:min-h-11 [&_label]:items-center">
                    <Switch checked={sounds.events[cue]} onCheckedChange={on => setEvent(cue, on)} label={t(`sounds.${cue}`)} />
                  </div>
                  <Button variant="ghost" className="size-11 shrink-0 p-0" disabled={!sounds.enabled || !sounds.events[cue] || sounds.volume === 0}
                    aria-label={t("sounds.preview", { event: t(`sounds.${cue}`) })} title={t("sounds.preview", { event: t(`sounds.${cue}`) })}
                    onClick={() => controller.previewSound(cue)}>
                    <Play aria-hidden className="size-4" />
                  </Button>
                </div>
              ))}
            </div>
          </section>
        ))}
      </fieldset>
    </div>
  );
}
