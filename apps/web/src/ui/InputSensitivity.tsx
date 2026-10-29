import * as Slider from "@radix-ui/react-slider";
import { useT } from "../i18n";
import { dbToLevel, MIN_INPUT_DB } from "../voice/input-sensitivity";

/** The live bar and keyboard/touch threshold share one dB scale. */
export function InputSensitivity({ level, threshold, onChange }: { level: number; threshold: number; onChange: (db: number) => void }) {
  const t = useT();
  const above = level > 0 && level >= dbToLevel(threshold);
  return (
    <Slider.Root className="slider-root min-h-11" value={[threshold]} min={MIN_INPUT_DB} max={0} step={1} onValueChange={([db]) => onChange(db ?? threshold)}>
      <Slider.Track className="relative h-3 grow rounded-full bg-active">
        <div role="meter" aria-label={t("audio.level")} aria-valuemin={MIN_INPUT_DB} aria-valuemax={0} aria-valuenow={Math.round(level * 100 + MIN_INPUT_DB)} className="absolute inset-0 overflow-hidden rounded-full">
          <div className={above ? "h-full bg-ok" : "h-full bg-warn"} style={{ width: `${level * 100}%` }} />
        </div>
      </Slider.Track>
      <Slider.Thumb className="sensitivity-thumb" aria-label={t("audio.sensitivity")} aria-valuetext={`${threshold} dB`} />
    </Slider.Root>
  );
}
