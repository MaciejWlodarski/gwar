import * as DialogPrimitive from "@radix-ui/react-dialog";
import { Bug, House, LayoutGrid, Server, Settings, UserRound, type LucideIcon } from "lucide-react";
import { useRef, useState, type PointerEvent } from "react";
import { useT, type Key } from "../i18n";
import { cn } from "../lib/cn";
import { profileTabs, useUi, type Section } from "../state/stores";
import { backInApp, focusPage } from "./hooks";
import { useInVoice, useServerUnread } from "./nav";

/** Every section of the app as the phone shows it, in the tab bar or as a tile in More. */
const SECTIONS: Record<Section, { label: Key; icon: LucideIcon }> = {
  home: { label: "nav.home", icon: House },
  servers: { label: "nav.servers", icon: Server },
  settings: { label: "settings.title", icon: Settings },
  bugReport: { label: "report.open", icon: Bug },
};

/** The sections pinned to the tab bar, in order: at most four. The others are tiles in More. */
const PINNED: readonly Section[] = ["home", "servers"];

/** More's tiles: the sections not pinned, then shortcuts into one of them (`profile`: the settings' profile tabs). */
const TILES: ReadonlyArray<{ id: string; label: Key; icon: LucideIcon; section: Section; open: () => void }> = [
  ...(Object.keys(SECTIONS) as Section[])
    .filter((section) => !PINNED.includes(section))
    .map((section) => ({ id: section, ...SECTIONS[section], section, open: () => openSection(section) })),
  { id: "profile", label: "nav.profile", icon: UserRound, section: "settings", open: () => backInApp(() => useUi.getState().openSettings("identity")) },
];

/** The tab bar's height without the safe area below it; the More sheet sits just above. */
const BAR = "58px";

/** A tab or a tile: to the section's top, as a way back when it is less deep than where the app is. */
function openSection(section: Section) {
  const ui = useUi.getState();
  backInApp(() => {
    if (section === "settings") ui.openSettings();
    // Tapping Servers again goes back to the channel list, as a tab bar's tab returns to its top.
    else if (section === "servers" && ui.section === "servers") ui.setPane("list");
    else ui.setSection(section);
  });
}

function Tab({ label, icon: Icon, current, onClick, badge = 0, voice, more }: { label: string; icon: LucideIcon; current: boolean; onClick: () => void; badge?: number; voice?: boolean; more?: boolean }) {
  return (
    <button
      onClick={onClick}
      data-more={more || undefined}
      aria-label={label}
      aria-current={current ? "page" : undefined}
      className={cn("t flex flex-1 cursor-pointer flex-col items-center justify-center gap-1 text-[11px] font-medium", current ? "text-accent" : "text-muted hover:text-fg")}
      style={{ height: BAR }}
    >
      <span className="relative">
        <Icon aria-hidden className="size-[22px]" />
        {badge > 0 && (
          <span className="absolute -top-1.5 -right-2.5 flex h-4 min-w-4 items-center justify-center rounded-full bg-danger px-1 text-[10px] leading-none font-semibold text-white ring-2 ring-rail">
            {badge > 99 ? "99+" : badge}
          </span>
        )}
        {voice && <span aria-hidden className="absolute -right-1 -bottom-0.5 size-[9px] rounded-full bg-ok ring-2 ring-rail" />}
      </span>
      <span>{label}</span>
    </button>
  );
}

/** Phones: the sections along the bottom of the screen, More holding the ones not pinned. */
export function TabBar() {
  const t = useT();
  const section = useUi((s) => s.section);
  const moreOpen = useUi((s) => s.moreOpen);
  const setMore = useUi((s) => s.setMore);
  const unread = useServerUnread();
  const inVoice = useInVoice();
  return (
    <nav aria-label={t("nav.sections")} className="flex shrink-0 items-stretch border-t border-line bg-rail pb-[env(safe-area-inset-bottom)]">
      {PINNED.map((s) => (
        <Tab
          key={s}
          label={t(SECTIONS[s].label)}
          icon={SECTIONS[s].icon}
          current={section === s && !moreOpen}
          onClick={() => openSection(s)}
          badge={s === "servers" ? unread : 0}
          voice={s === "servers" && inVoice}
        />
      ))}
      <Tab label={t("nav.more")} icon={LayoutGrid} current={moreOpen || !PINNED.includes(section)} onClick={() => setMore(!moreOpen)} more />
    </nav>
  );
}

/** More: a sheet over the tab bar with a tile for every section not pinned. Closed by a tap outside, Esc, a swipe down or a pick. */
export function MoreSheet() {
  const t = useT();
  const open = useUi((s) => s.moreOpen);
  const setMore = useUi((s) => s.setMore);
  const section = useUi((s) => s.section);
  const profile = useUi((s) => s.section === "settings" && s.settingsDrill && profileTabs.includes(s.settingsTab));
  // A swipe down on the handle: the sheet follows the finger and closes past a third of its height.
  const [drag, setDrag] = useState(0);
  const start = useRef<number | null>(null);
  const sheet = useRef<HTMLDivElement>(null);
  // A tile was picked: focus goes to the page it opened, not back to More.
  const picked = useRef(false);
  const down = (e: PointerEvent) => {
    start.current = e.clientY;
    e.currentTarget.setPointerCapture(e.pointerId);
  };
  const move = (e: PointerEvent) => {
    if (start.current !== null) setDrag(Math.max(0, e.clientY - start.current));
  };
  const up = () => {
    if (start.current !== null && drag > (sheet.current?.offsetHeight ?? 0) / 3) setMore(false);
    start.current = null;
    setDrag(0);
  };
  return (
    <DialogPrimitive.Root open={open} onOpenChange={setMore}>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Overlay className="anim-fade fixed inset-x-0 top-0 z-40 bg-black/55" style={{ bottom: `calc(${BAR} + env(safe-area-inset-bottom))` }} />
        <DialogPrimitive.Content
          ref={sheet}
          aria-describedby={undefined}
          aria-modal="true"
          onOpenAutoFocus={() => void (picked.current = false)}
          onCloseAutoFocus={(e) => {
            // The More tab opened the sheet (it is no Radix trigger, so Radix would leave focus on the page's body).
            e.preventDefault();
            if (picked.current) focusPage();
            else document.querySelector<HTMLElement>("[data-more]")?.focus();
          }}
          className="anim-sheet fixed inset-x-0 z-40 rounded-t-2xl border-t border-line bg-side px-4 pb-4 outline-none"
          style={{ bottom: `calc(${BAR} + env(safe-area-inset-bottom))`, transform: drag ? `translateY(${drag}px)` : undefined }}
        >
          <div onPointerDown={down} onPointerMove={move} onPointerUp={up} onPointerCancel={up} className="flex cursor-grab touch-none justify-center py-2.5">
            <span aria-hidden className="h-1 w-10 rounded-full bg-line-strong" />
          </div>
          <DialogPrimitive.Title className="mb-3 text-base font-semibold">{t("nav.more")}</DialogPrimitive.Title>
          <div className="grid grid-cols-3 gap-2">
            {TILES.map(({ id, label, icon: Icon, section: of, open: go }) => {
              const here = id === "profile" ? profile : section === of && !profile;
              return (
                <button
                  key={id}
                  onClick={() => {
                    picked.current = true;
                    go();
                  }}
                  aria-current={here ? "page" : undefined}
                  className={cn(
                    "t flex h-20 cursor-pointer flex-col items-center justify-center gap-2 rounded-[14px] bg-raised px-1 text-center text-xs font-medium",
                    here ? "text-accent ring-1 ring-accent/50" : "text-fg hover:bg-hover",
                  )}
                >
                  <Icon aria-hidden className="size-[22px]" />
                  <span className="leading-tight">{t(label)}</span>
                </button>
              );
            })}
          </div>
        </DialogPrimitive.Content>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  );
}
