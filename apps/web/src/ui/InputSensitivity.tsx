import * as Slider from "@radix-ui/react-slider";
import { useId } from "react";
import { useT } from "../i18n";
import { dbToLevel, MIN_INPUT_DB } from "../voice/input-sensitivity";

/** The meter and threshold share one dB scale, including with keyboard or touch input. */
export function InputSensitivity({ level, threshold, onChange }: { level: number; threshold: number; onChange: (db: number) => void }) {
  const t = useT();
  const hint = useId();
  const above = level > 0 && level >= dbToLevel(threshold);
  return (
    <div className="flex flex-col gap-1">
      <div className="flex items-center justify-between gap-3 text-sm">
        <span>{t("audio.sensitivity")}</span>
        <span className="shrink-0 tabular-nums">{threshold} dB</span>
      </div>
      <Slider.Root className="slider-root min-h-11" value={[threshold]} min={MIN_INPUT_DB} max={0} step={1} onValueChange={([db]) => onChange(db ?? threshold)}>
        <Slider.Track className="relative h-3 grow rounded-full bg-active">
          <div role="meter" aria-label={t("audio.level")} aria-valuemin={MIN_INPUT_DB} aria-valuemax={0} aria-valuenow={Math.round(level * 100 + MIN_INPUT_DB)} className="absolute inset-0 overflow-hidden rounded-full">
            <div className={above ? "h-full bg-ok" : "h-full bg-warn"} style={{ width: `${level * 100}%` }} />
          </div>
        </Slider.Track>
        <Slider.Thumb className="sensitivity-thumb" aria-label={t("audio.sensitivity")} aria-valuetext={`${threshold} dB`} aria-describedby={hint} />
      </Slider.Root>
      <div className="flex justify-between text-xs tabular-nums text-muted" aria-hidden="true"><span>−100 dB</span><span>0 dB</span></div>
      <p className="text-xs text-muted" id={hint}>{t("audio.sensitivityHint")}</p>
      <p className="text-sm text-fg">{t(above ? "audio.aboveThreshold" : "audio.belowThreshold")}</p>
    </div>
  );
}
