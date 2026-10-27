import { Bug, House, Server, Settings } from "lucide-react";
import type { ReactNode } from "react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { useAccountIdentity } from "../state/identity";
import { profileTabs, useUi, type Section } from "../state/stores";
import { BrandMark } from "./Brand";
import { AlphaBadge } from "./BugReport";
import { Avatar, Tooltip } from "./kit";
import { useInVoice, useServerUnread } from "./nav";
import { UpdateButton, VersionLabel } from "./Version";

function SectionButton({ section, label, icon, badge, voice }: { section: Section; label: string; icon: ReactNode; badge?: number; voice?: boolean }) {
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
        {/* In a voice channel: a dot below, clear of the unread count above. */}
        {voice && <span aria-hidden className="absolute right-2 bottom-2 size-[9px] rounded-full bg-ok ring-2 ring-rail" />}
      </button>
    </Tooltip>
  );
}

/** The app's own pages at the bottom of the rail: smaller than the sections above, but marked the same way when open. */
function PageButton({ label, current, onClick, children }: { label: string; current: boolean; onClick: () => void; children: ReactNode }) {
  return (
    <Tooltip label={label} side="right">
      <button
        onClick={onClick}
        aria-label={label}
        aria-current={current ? "page" : undefined}
        className={cn(
          "t flex size-8 shrink-0 cursor-pointer items-center justify-center rounded-md",
          current ? "bg-accent-soft text-accent" : "text-muted hover:bg-hover hover:text-fg",
        )}
      >
        {children}
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
  const inVoice = useInVoice();
  const section = useUi((s) => s.section);
  const profile = useUi((s) => profileTabs.includes(s.settingsTab));
  const setSection = useUi((s) => s.setSection);
  const openSettings = useUi((s) => s.openSettings);

  return (
    <nav aria-label={t("nav.sections")} className="flex h-full w-16 shrink-0 flex-col items-center gap-2 bg-rail py-3">
      <div aria-hidden className="mb-1 flex h-8 items-center justify-center">
        <BrandMark className="h-5 w-auto" />
      </div>
      <SectionButton section="home" label={t("nav.home")} icon={<House className="size-5" />} />
      <SectionButton section="servers" label={t("nav.servers")} icon={<Server className="size-5" />} badge={unread} voice={inVoice} />
      <div className="flex-1" />
      <UpdateButton />
      <PageButton label={t("report.open")} current={section === "bugReport"} onClick={() => setSection("bugReport")}>
        <Bug className="size-5" />
      </PageButton>
      <PageButton label={t("settings.title")} current={section === "settings" && !profile} onClick={() => openSettings()}>
        <Settings className="size-5" />
      </PageButton>
      <Tooltip label={t("nav.account")} side="right">
        <button
          onClick={() => openSettings("identity")}
          aria-label={t("nav.account")}
          aria-current={section === "settings" && profile ? "page" : undefined}
          className={cn("t mt-1 cursor-pointer rounded-full", section === "settings" && profile ? "ring-2 ring-accent" : "ring-accent/40 hover:ring-2")}
        >
          <Avatar name={nickname || "?"} seed={nickname} size={32} />
        </button>
      </Tooltip>
      <VersionLabel />
      <AlphaBadge />
    </nav>
  );
}
