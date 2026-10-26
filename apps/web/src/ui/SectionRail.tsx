import { House, Server, Settings } from "lucide-react";
import type { ReactNode } from "react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { useAccountIdentity } from "../state/identity";
import { useUi, type Section } from "../state/stores";
import { BrandMark } from "./Brand";
import { AlphaBadge, BugReportButton } from "./BugReport";
import { Avatar, IconButton, Tooltip } from "./kit";
import { useServerUnread } from "./nav";
import { UpdateButton, VersionLabel } from "./Version";

function SectionButton({ section, label, icon, badge }: { section: Section; label: string; icon: ReactNode; badge?: number }) {
  const current = useUi((s) => s.section === section);
  const setSection = useUi((s) => s.setSection);
  return (
    <Tooltip label={label} side="right">
      <button
        onClick={() => setSection(section)}
        aria-label={label}
        aria-current={current ? "page" : undefined}
        className={cn(
          "t relative flex size-11 shrink-0 cursor-pointer items-center justify-center rounded-xl",
          current ? "bg-accent-soft text-accent" : "text-subtle hover:bg-hover hover:text-fg",
        )}
      >
        {icon}
        {!!badge && (
          <span className="absolute -top-0.5 -right-0.5 flex h-4 min-w-4 items-center justify-center rounded-full bg-danger px-1 text-[10px] leading-none font-semibold text-white ring-2 ring-rail">
            {badge > 99 ? "99+" : badge}
          </span>
        )}
      </button>
    </Tooltip>
  );
}

/**
 * The global rail, left of everything: the app's sections on top, the app itself
 * (update, bug report, settings, the account) at the bottom. Grey, unlike the
 * coloured server icons beside it.
 */
export function SectionRail() {
  const t = useT();
  const unread = useServerUnread();
  const nickname = useAccountIdentity((s) => s.nickname);
  const openDialog = useUi((s) => s.openDialog);

  return (
    <nav aria-label={t("nav.sections")} className="flex h-full w-16 shrink-0 flex-col items-center gap-2 bg-rail py-3">
      <div aria-hidden className="mb-1 flex h-8 items-center justify-center">
        <BrandMark className="h-5 w-auto" />
      </div>
      <SectionButton section="home" label={t("nav.home")} icon={<House className="size-5" />} />
      <SectionButton section="servers" label={t("nav.servers")} icon={<Server className="size-5" />} badge={unread} />
      <div className="flex-1" />
      <UpdateButton />
      <BugReportButton />
      <IconButton label={t("settings.title")} side="right" onClick={() => openDialog({ kind: "settings", tab: "audio" })}>
        <Settings className="size-5" />
      </IconButton>
      <Tooltip label={t("nav.account")} side="right">
        <button
          onClick={() => openDialog({ kind: "settings", tab: "identity" })}
          aria-label={t("nav.account")}
          className="t mt-1 cursor-pointer rounded-full ring-accent/40 hover:ring-2"
        >
          <Avatar name={nickname || "?"} seed={nickname} size={32} />
        </button>
      </Tooltip>
      <VersionLabel />
      <AlphaBadge />
    </nav>
  );
}

/** The same sections as tabs along the bottom of the mobile drawer. */
export function SectionTabs() {
  const t = useT();
  const section = useUi((s) => s.section);
  const setSection = useUi((s) => s.setSection);
  const setDrawer = useUi((s) => s.setDrawer);
  const openDialog = useUi((s) => s.openDialog);
  const unread = useServerUnread();

  const tab = (key: Section, label: string, icon: ReactNode, badge = 0) => (
    <button
      key={key}
      onClick={() => {
        setSection(key);
        if (key === "home") setDrawer(false);
      }}
      aria-label={label}
      aria-current={section === key ? "page" : undefined}
      className={cn(
        "t relative flex h-12 flex-1 cursor-pointer flex-col items-center justify-center gap-0.5 text-[11px]",
        section === key ? "text-accent" : "text-muted hover:text-fg",
      )}
    >
      {icon}
      <span>{label}</span>
      {badge > 0 && <span className="absolute top-1.5 left-1/2 ml-2 size-2 rounded-full bg-danger" />}
    </button>
  );

  return (
    <nav aria-label={t("nav.sections")} className="flex shrink-0 items-stretch border-t border-line bg-rail px-1">
      {tab("home", t("nav.home"), <House className="size-5" />)}
      {tab("servers", t("nav.servers"), <Server className="size-5" />, unread)}
      <button
        onClick={() => openDialog({ kind: "settings", tab: "audio" })}
        aria-label={t("settings.title")}
        className="t flex h-12 flex-1 cursor-pointer flex-col items-center justify-center gap-0.5 text-[11px] text-muted hover:text-fg"
      >
        <Settings className="size-5" />
        <span>{t("settings.title")}</span>
      </button>
    </nav>
  );
}
