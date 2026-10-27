import * as DialogPrimitive from "@radix-ui/react-dialog";
import * as Tabs from "@radix-ui/react-tabs";
import { ChevronRight, X } from "lucide-react";
import { useEffect, useRef, type ReactNode } from "react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { useIsMobile } from "../lib/media";
import { useUi } from "../state/stores";
import { leavePage, useEscapeBack } from "./hooks";
import { BackButton, IconButton, panelCard } from "./kit";
import { useVoicePill } from "./nav";
import { VoicePill } from "./VoicePanel";

export interface SettingsPage<T extends string> {
  id: T;
  label: string;
  icon: ReactNode;
  /** Lists and two-column editors use the full width; forms stay readable at a narrower one. */
  wide?: boolean;
  /** Phones: the setting's current value, shown in the list of pages. */
  value?: string;
  /** Phones: pages of a group are listed together. */
  group?: number;
}

const tabTrigger =
  "t flex h-9 shrink-0 cursor-pointer focus-visible:outline-offset-[-2px] items-center gap-2.5 rounded-md px-2.5 text-left text-sm text-muted hover:bg-hover hover:text-fg data-[state=active]:bg-active data-[state=active]:font-medium data-[state=active]:text-fg [&>svg]:shrink-0";

/** A settings window of a fixed size: pages on the left (on top on phones), the open page on the right.
 *  The size never depends on the page, so switching pages keeps the navigation where it was. */
export function SettingsLayout<T extends string>({
  title,
  pages,
  value,
  onValueChange,
  onClose,
  children,
}: {
  title: string;
  pages: Array<SettingsPage<T>>;
  value: T;
  onValueChange: (v: T) => void;
  onClose: () => void;
  children: (page: T) => ReactNode;
}) {
  const t = useT();
  const isMobile = useIsMobile();
  const current = pages.find((p) => p.id === value);
  return (
    <DialogPrimitive.Root open onOpenChange={(o) => !o && onClose()}>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Overlay className="anim-fade fixed inset-0 z-50 bg-black/55 backdrop-blur-[2px]" />
        <DialogPrimitive.Content
          aria-describedby={undefined}
          onOpenAutoFocus={(e) => {
            // Start on the open page's tab, not on the close button (and its tooltip).
            e.preventDefault();
            (e.currentTarget as HTMLElement).querySelector<HTMLElement>('[role="tab"][data-state="active"]')?.focus();
          }}
          className="anim-pop fixed inset-0 z-50 flex flex-col overflow-hidden bg-surface outline-none md:top-1/2 md:left-1/2 md:h-[min(720px,calc(100dvh-48px))] md:w-[min(960px,calc(100vw-48px))] md:-translate-x-1/2 md:-translate-y-1/2 md:rounded-xl md:border md:border-line-strong md:shadow-pop"
        >
          <Tabs.Root
            value={value}
            onValueChange={(v) => onValueChange(v as T)}
            orientation={isMobile ? "horizontal" : "vertical"}
            className="flex min-h-0 flex-1 flex-col md:flex-row"
          >
            <div className="flex shrink-0 flex-col border-line bg-side max-md:border-b md:w-56 md:border-r">
              <div className="flex h-14 shrink-0 items-center justify-between gap-2 px-4 md:px-5">
                <DialogPrimitive.Title className="truncate text-base font-semibold text-fg">{title}</DialogPrimitive.Title>
                <DialogPrimitive.Close asChild>
                  <IconButton label={t("common.close")} tooltip={false} size="sm" className="md:hidden">
                    <X className="size-4" />
                  </IconButton>
                </DialogPrimitive.Close>
              </div>
              <Tabs.List
                aria-label={title}
                className="flex gap-1 overflow-x-auto px-3 pb-3 md:min-h-0 md:flex-1 md:flex-col md:overflow-y-auto md:overflow-x-hidden"
              >
                {pages.map((p) => (
                  <Tabs.Trigger
                    key={p.id}
                    value={p.id}
                    className={tabTrigger}
                  >
                    {p.icon}
                    <span className="truncate">{p.label}</span>
                  </Tabs.Trigger>
                ))}
              </Tabs.List>
            </div>
            <div className="flex min-h-0 min-w-0 flex-1 flex-col">
              <div className="hidden h-14 shrink-0 items-center justify-between gap-2 border-b border-line px-6 md:flex">
                <h2 className="truncate text-base font-semibold text-fg">{current?.label}</h2>
                <DialogPrimitive.Close asChild>
                  <IconButton label={t("common.close")} tooltip={false} size="sm">
                    <X className="size-4" />
                  </IconButton>
                </DialogPrimitive.Close>
              </div>
              <div className="min-h-0 flex-1 overflow-y-auto [scrollbar-gutter:stable]">
                {pages.map((p) => (
                  <Tabs.Content key={p.id} value={p.id} className={cn("px-4 py-5 outline-none md:px-6", !p.wide && "max-w-[720px]")}>
                    {children(p.id)}
                  </Tabs.Content>
                ))}
              </div>
            </div>
          </Tabs.Root>
        </DialogPrimitive.Content>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  );
}

interface PageLayoutProps<T extends string> {
  title: string;
  icon: ReactNode;
  pages: Array<SettingsPage<T>>;
  value: T;
  onValueChange: (v: T) => void;
  card?: boolean;
  /** Phones: below the list of pages. */
  footer?: ReactNode;
  children: (page: T) => ReactNode;
}

/**
 * Settings as a section of the app: the pages in a card of their own where the server column and channel
 * panel stand in the server view, the open page in the main card; on phones a list of the pages, each
 * opening over it. It takes focus when it opens; Esc goes back to the section it was opened from, and
 * focus with it.
 */
export function SettingsPageLayout<T extends string>(props: PageLayoutProps<T>) {
  return useIsMobile() ? <SettingsPageList {...props} /> : <SettingsPageCards {...props} />;
}

function SettingsPageCards<T extends string>({ title, icon, pages, value, onValueChange, card, children }: PageLayoutProps<T>) {
  const pill = useVoicePill();
  const current = pages.find((p) => p.id === value);
  const list = useRef<HTMLDivElement>(null);
  useEscapeBack();
  // Opening the page moves focus into it, onto the open tab, as the dialog did.
  useEffect(() => list.current?.querySelector<HTMLElement>('[role="tab"][data-state="active"]')?.focus(), []);
  return (
    <Tabs.Root value={value} onValueChange={(v) => onValueChange(v as T)} orientation="vertical" className="flex min-w-0 flex-1 gap-2">
      <div className={cn("flex w-[280px] shrink-0 flex-col bg-side", card && panelCard)}>
        <div className="flex h-14 shrink-0 items-center gap-2 px-5">
          <span className="text-subtle [&>svg]:size-5">{icon}</span>
          <h1 className="truncate text-base font-semibold text-fg">{title}</h1>
        </div>
        <Tabs.List ref={list} aria-label={title} className="flex min-h-0 flex-1 flex-col gap-1 overflow-x-hidden overflow-y-auto px-3 pb-3">
          {pages.map((p) => (
            <Tabs.Trigger key={p.id} value={p.id} className={tabTrigger}>
              {p.icon}
              <span className="truncate">{p.label}</span>
            </Tabs.Trigger>
          ))}
        </Tabs.List>
      </div>
      <main aria-label={title} className={cn("relative flex min-h-0 min-w-0 flex-1 flex-col bg-surface", card && panelCard)}>
        <header className="flex h-14 shrink-0 items-center gap-2 border-b border-line px-6">
          <h2 className="truncate text-base font-semibold text-fg">{current?.label}</h2>
        </header>
        <div className={cn("min-h-0 flex-1 overflow-y-auto [scrollbar-gutter:stable]", pill && "pb-20")}>
          {pages.map((p) => (
            <Tabs.Content key={p.id} value={p.id} className={cn("px-4 py-5 outline-none md:px-6", !p.wide && "max-w-[720px]")}>
              {children(p.id)}
            </Tabs.Content>
          ))}
        </div>
        <VoicePill />
      </main>
    </Tabs.Root>
  );
}

/**
 * Phones: the pages as a grouped list (icon, name, the current value where there is one), and the open
 * page over it with a way back. Back (Esc, the system's back) leaves the open page first, then Settings.
 */
function SettingsPageList<T extends string>({ title, icon, pages, value, onValueChange, footer, children }: PageLayoutProps<T>) {
  const pill = useVoicePill();
  const drill = useUi((s) => s.settingsDrill);
  const setDrill = useUi((s) => s.setSettingsDrill);
  const current = pages.find((p) => p.id === value);
  const main = useRef<HTMLElement>(null);
  useEscapeBack(() => (useUi.getState().settingsDrill ? setDrill(false) : leavePage()));
  // Focus follows: onto the open page's way back, or back onto its row of the list.
  const opened = useRef(false);
  useEffect(() => {
    if (!opened.current && !drill) return void (opened.current = true);
    opened.current = true;
    main.current?.querySelector<HTMLElement>(drill ? "header button" : `[data-page="${value}"]`)?.focus();
  }, [drill, value]);
  const groups = [...new Set(pages.map((p) => p.group ?? 0))].map((g) => pages.filter((p) => (p.group ?? 0) === g));
  return (
    <main ref={main} aria-label={title} className="relative flex min-h-0 min-w-0 flex-1 flex-col bg-surface">
      {drill && current ? (
        <>
          <header className="flex h-12 shrink-0 items-center gap-1 border-b border-line px-3">
            <BackButton text onClick={() => setDrill(false)} />
            <h2 className="min-w-0 truncate text-[15px] font-semibold">{current.label}</h2>
          </header>
          <div className={cn("min-h-0 flex-1 overflow-y-auto px-4 py-5", pill && "pb-20")}>{children(current.id)}</div>
        </>
      ) : (
        <>
          <header className="flex h-12 shrink-0 items-center gap-2 border-b border-line px-4">
            <span className="text-subtle [&>svg]:size-5">{icon}</span>
            <h1 className="truncate text-[15px] font-semibold">{title}</h1>
          </header>
          <div className={cn("min-h-0 flex-1 overflow-y-auto px-4 py-4", pill && "pb-20")}>
            {groups.map((group, i) => (
              <ul key={i} className="mb-5 divide-y divide-line overflow-hidden rounded-xl border border-line bg-side">
                {group.map((p) => (
                  <li key={p.id}>
                    <button
                      data-page={p.id}
                      onClick={() => {
                        onValueChange(p.id);
                        setDrill(true);
                      }}
                      className="t flex h-12 w-full cursor-pointer items-center gap-3 px-3 text-left text-sm hover:bg-hover"
                    >
                      <span aria-hidden className="flex size-7 shrink-0 items-center justify-center rounded-md bg-raised text-muted">
                        {p.icon}
                      </span>
                      <span className="min-w-0 flex-1 truncate font-medium">{p.label}</span>
                      {p.value && <span className="max-w-[45%] truncate text-muted">{p.value}</span>}
                      <ChevronRight aria-hidden className="size-4 shrink-0 text-subtle" />
                    </button>
                  </li>
                ))}
              </ul>
            ))}
            {footer}
          </div>
        </>
      )}
      <VoicePill />
    </main>
  );
}
