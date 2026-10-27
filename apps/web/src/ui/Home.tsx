import { ArrowRight, House, Menu, Plus, Volume2 } from "lucide-react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { useIsMobile } from "../lib/media";
import { useAccountIdentity } from "../state/identity";
import { myChannelId } from "../state/reducer";
import { useBookmarks } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import { TeamSpeakBadge } from "./badges";
import { Avatar, Button, IconButton, panelCard } from "./kit";
import { useBookmarkOpener, useVoicePill } from "./nav";
import { VoicePill } from "./VoicePanel";

/**
 * The home section: where I am connected, and every saved server one click away.
 * The server column and the channel panel step aside for it.
 */
export function Home({ card }: { card?: boolean }) {
  const t = useT();
  const isMobile = useIsMobile();
  const nickname = useAccountIdentity((s) => s.nickname);
  const bookmarks = useBookmarks();
  const phase = useSession((s) => s.phase);
  const serverName = useSession((s) => s.server?.name);
  const voiceChannel = useSession((s) => {
    const id = myChannelId(s);
    return id === null ? null : (s.channels[id]?.name ?? null);
  });
  const setSection = useUi((s) => s.setSection);
  const setDrawer = useUi((s) => s.setDrawer);
  const openDialog = useUi((s) => s.openDialog);
  const { isActive, open } = useBookmarkOpener();
  const connected = phase === "online" || phase === "reconnecting";
  const pill = useVoicePill();

  return (
    <main className={cn("relative flex min-w-0 flex-1 flex-col bg-surface", card && panelCard)}>
      <header className="flex h-12 shrink-0 items-center gap-2 border-b border-line px-3 md:px-4">
        {isMobile && (
          <IconButton label={t("nav.channels")} onClick={() => setDrawer(true)} className="-ml-1">
            <Menu className="size-5" />
          </IconButton>
        )}
        <House className="size-5 text-subtle" />
        <h2 className="truncate text-[15px] font-semibold">{t("nav.home")}</h2>
      </header>
      <div className={cn("min-h-0 flex-1 overflow-y-auto", pill && "pb-20")}>
        <div className="mx-auto flex w-full max-w-4xl flex-col gap-6 p-4 md:p-8">
          <h1 className="text-2xl font-semibold tracking-tight">{nickname ? t("home.greeting", { name: nickname }) : t("home.greetingAnon")}</h1>

          <section className="flex flex-wrap items-center gap-4 rounded-2xl border border-line bg-[linear-gradient(135deg,var(--accent-soft),transparent_60%)] p-5">
            <div className="min-w-[12rem] flex-1">
              <div className="flex items-center gap-2 text-xs font-semibold tracking-wide text-subtle uppercase">
                <span className={cn("size-2 rounded-full", connected ? (phase === "reconnecting" ? "bg-warn" : "bg-ok") : "bg-subtle")} />
                {connected ? t("home.connected") : t("home.notConnected")}
              </div>
              <div className="mt-1 truncate text-lg font-semibold">{connected ? serverName : t("home.pickServer")}</div>
              {connected && (
                <div className="mt-0.5 flex items-center gap-1.5 text-sm text-muted">
                  <Volume2 className="size-4 shrink-0" />
                  <span className="truncate">{voiceChannel ?? t("home.notInVoice")}</span>
                </div>
              )}
            </div>
            <Button variant="primary" onClick={() => setSection("servers")}>
              {connected ? t("home.openServer") : t("home.connect")} <ArrowRight className="size-4" />
            </Button>
          </section>

          <section>
            <h3 className="mb-3 text-xs font-semibold tracking-wide text-subtle uppercase">{t("home.saved")}</h3>
            <div className="grid grid-cols-[repeat(auto-fill,minmax(220px,1fr))] gap-3">
              {bookmarks.map((b) => {
                const active = isActive(b);
                return (
                  <button
                    key={b.id}
                    onClick={() => open(b)}
                    className={cn(
                      "t flex cursor-pointer items-center gap-3 rounded-xl border bg-side p-3 text-left hover:border-line-strong hover:bg-raised",
                      active ? "border-accent/60" : "border-line",
                    )}
                  >
                    <Avatar name={b.name} seed={b.address} size={40} square />
                    <span className="min-w-0 flex-1">
                      <span className="flex items-center gap-1.5">
                        <span className="truncate text-sm font-medium">{b.name}</span>
                        {b.kind === "teamspeak" && <TeamSpeakBadge short />}
                      </span>
                      <span className="block truncate text-xs text-subtle">{active ? t("home.current") : b.address}</span>
                    </span>
                  </button>
                );
              })}
              <button
                onClick={() => openDialog({ kind: "addServer" })}
                className="t flex min-h-16 cursor-pointer items-center justify-center gap-2 rounded-xl border border-dashed border-line-strong p-3 text-sm text-muted hover:border-accent hover:text-accent"
              >
                <Plus className="size-4" /> {t("rail.add")}
              </button>
            </div>
          </section>
        </div>
      </div>
      <VoicePill />
    </main>
  );
}
