import * as DialogPrimitive from "@radix-ui/react-dialog";
import * as Tabs from "@radix-ui/react-tabs";
import { X } from "lucide-react";
import type { ReactNode } from "react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { useIsMobile } from "../lib/media";
import { IconButton } from "./kit";

export interface SettingsPage<T extends string> {
  id: T;
  label: string;
  icon: ReactNode;
  /** Lists and two-column editors use the full width; forms stay readable at a narrower one. */
  wide?: boolean;
}

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
                  <IconButton label={t("common.close")} size="sm" className="md:hidden">
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
                    className="t flex h-9 shrink-0 cursor-pointer focus-visible:outline-offset-[-2px] items-center gap-2.5 rounded-md px-2.5 text-left text-sm text-muted hover:bg-hover hover:text-fg data-[state=active]:bg-active data-[state=active]:font-medium data-[state=active]:text-fg [&>svg]:shrink-0"
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
                  <IconButton label={t("common.close")} size="sm">
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
