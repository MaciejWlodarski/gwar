import { Mouse as MouseIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { mousePttCode, useGlobalPtt } from "../platform/ptt";
import { useSettings } from "../state/settings";
import { keyLabel } from "./hooks";
import { Button, Dialog, IconButton, Slider } from "./kit";
import { PttStatus } from "./PttStatus";

/** Mounted only while these controls are visible; unmount retires every recorder. */
export function PttControls({ disabled: gateUnsupported }: { disabled: boolean }) {
  const t = useT();
  const audio = useSettings(s => s.audio);
  const setAudio = useSettings(s => s.setAudio);
  const [capturing, setCapturing] = useState(false);
  const [pickingMouse, setPickingMouse] = useState(false);
  useEffect(() => {
    if (!capturing) return;
    useGlobalPtt.setState({ capturing: true });
    const finish = (key: string | null) => {
      if (key) setAudio({ pttKey: key });
      setCapturing(false);
    };
    const onKey = (e: KeyboardEvent) => {
      e.preventDefault(); e.stopPropagation();
      finish(e.code === "Escape" ? null : e.code);
    };
    const onMouse = (e: MouseEvent) => {
      const code = mousePttCode(e.button);
      if (!code) return;
      e.preventDefault(); e.stopPropagation(); finish(code);
    };
    const cancel = () => setCapturing(false);
    window.addEventListener("keydown", onKey, true);
    window.addEventListener("mousedown", onMouse, true);
    window.addEventListener("blur", cancel);
    return () => {
      useGlobalPtt.setState({ capturing: false });
      window.removeEventListener("keydown", onKey, true);
      window.removeEventListener("mousedown", onMouse, true);
      window.removeEventListener("blur", cancel);
    };
  }, [capturing, setAudio]);

  return <>
    <Dialog open={pickingMouse} onOpenChange={setPickingMouse} title={t("audio.mouseButton")}>
      <div className="grid gap-2">
        {["Mouse3", "Mouse4", "Mouse5"].map(key => <Button key={key} className="h-11 justify-start" onClick={() => { setAudio({ pttKey: key }); setPickingMouse(false); }}>{keyLabel(key)}</Button>)}
      </div>
    </Dialog>
    <div className="flex items-center gap-2">
      <Button disabled={gateUnsupported} onClick={() => setCapturing(true)} className={cn("min-w-40", capturing && "border-accent text-accent")}>
        {capturing ? t("audio.pressPtt") : audio.pttKey ? keyLabel(audio.pttKey) : t("audio.setKey")}
      </Button>
      <IconButton disabled={gateUnsupported} size="lg" label={t("audio.mouseButton")} onClick={() => { setCapturing(false); setPickingMouse(true); }}><MouseIcon className="size-4" /></IconButton>
      {audio.pttKey && !capturing && <Button variant="ghost" onClick={() => setAudio({ pttKey: null })}>{t("audio.clearKey")}</Button>}
    </div>
    {gateUnsupported || !audio.pttKey ? <p className="self-center text-xs text-subtle">{t(gateUnsupported ? "audio.pttUnsupported" : "audio.pttNoKey")}</p> : <PttStatus />}
    <div className="flex items-center gap-3 text-xs">
      <span className="shrink-0 text-subtle">{t("audio.pttDelay")}</span>
      <Slider value={audio.pttReleaseDelay} min={0} max={1000} step={10} onChange={(pttReleaseDelay) => setAudio({ pttReleaseDelay })} label={t("audio.pttDelayLabel")} />
      <span className="w-14 shrink-0 text-right tabular-nums text-muted">{audio.pttReleaseDelay} ms</span>
    </div>
  </>;
}
