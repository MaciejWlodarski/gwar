import { useEffect } from "react";
import { useT } from "../i18n";
import { controller } from "../state/controller";
import { useSettings } from "../state/settings";
import { useVoice } from "../state/stores";
import { Field, Section, Select, Slider } from "./kit";

const DEFAULT = "__default";

/** Identical choices, fallback and capability handling in Settings and the voice panel. */
export function AudioDevices({ compact = false }: { compact?: boolean }) {
  const t = useT();
  const audio = useSettings(s => s.audio);
  const setAudio = useSettings(s => s.setAudio);
  const devices = useVoice(s => s.devices);
  const engine = controller.voice;
  useEffect(() => {
    let alive = true;
    void engine.listDevices().then(devices => { if (alive) useVoice.getState().set({ devices }); }).catch(() => {});
    return () => { alive = false; };
  }, [engine]);
  const withDefault = (list: Array<{ id: string; label: string }>) => [
    { value: DEFAULT, label: t("audio.systemDefault") },
    ...list.map((d) => ({ value: d.id, label: d.label })),
  ];
  const controls = (
    <>
        <Field label={t("audio.input")}>
          {(id) => (
            <Select
              id={id}
              value={audio.inputDeviceId && devices.inputs.some((d) => d.id === audio.inputDeviceId) ? audio.inputDeviceId : DEFAULT}
              onValueChange={(v) => setAudio({ inputDeviceId: v === DEFAULT ? null : v })}
              options={withDefault(devices.inputs)}
            />
          )}
        </Field>
        <Field label={t("audio.output")} hint={engine.capabilities.outputDeviceSelection ? undefined : t("audio.outputUnsupported")}>
          {(id) => (
            <Select
              id={id}
              disabled={!engine.capabilities.outputDeviceSelection}
              value={audio.outputDeviceId && devices.outputs.some((d) => d.id === audio.outputDeviceId) ? audio.outputDeviceId : DEFAULT}
              onValueChange={(v) => setAudio({ outputDeviceId: v === DEFAULT ? null : v })}
              options={withDefault(devices.outputs)}
            />
          )}
        </Field>
        <Field label={t("audio.masterVolume")}>
          {() => (
            <div className="flex items-center gap-3">
              <Slider
                value={Math.round(audio.masterVolume * 100)}
                min={0}
                max={150}
                step={5}
                onChange={(v) => setAudio({ masterVolume: v / 100 })}
                label={t("audio.masterVolume")}
              />
              <span className="w-12 shrink-0 text-right text-sm tabular-nums text-muted">{Math.round(audio.masterVolume * 100)}%</span>
            </div>
          )}
        </Field>
    </>
  );
  return compact ? <div className="flex flex-col gap-3">{controls}</div> : <Section title={t("audio.devices")}>{controls}</Section>;
}
