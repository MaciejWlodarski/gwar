import * as DialogPrimitive from "@radix-ui/react-dialog";
import { useEffect } from "react";
import { useT } from "./i18n";
import { accountActions } from "./state/account";
import { cn } from "./lib/cn";
import { useIsMobile } from "./lib/media";
import { initDesktop } from "./platform/desktop";
import { initUpdates } from "./platform/updater";
import { controller } from "./state/controller";
import { useSession, useUi } from "./state/stores";
import { ChatArea } from "./ui/Chat";
import { ConnectionWarning } from "./ui/ConnectionWarning";
import { ConnectScreen } from "./ui/ConnectScreen";
import { Dialogs } from "./ui/Dialogs";
import { useShortcuts, useTheme } from "./ui/hooks";
import { TooltipProvider } from "./ui/kit";
import { MemberList } from "./ui/MemberList";
import { useMembersPanel } from "./ui/members";
import { Home } from "./ui/Home";
import { Overview } from "./ui/Overview";
import { useServerUnread } from "./ui/nav";
import { SectionRail, SectionTabs } from "./ui/SectionRail";
import { ServerRail } from "./ui/ServerRail";
import { SharePresetDialog, SharingBar, Stage } from "./ui/Screen";
import { useScreen } from "./state/screen";
import { Sidebar } from "./ui/Sidebar";
import { Toaster } from "./ui/Toaster";

export function App() {
  const t = useT();
  const isMobile = useIsMobile();
  const insecureConnection = useSession((s) => s.insecureConnection);
  const phase = useSession((s) => s.phase);
  const serverName = useSession((s) => s.server?.name);
  const unread = useServerUnread();
  const address = useSession((s) => s.address);
  const home = useUi((s) => s.section === "home");
  // With a share on stage the stage wins; the overview waits until it closes.
  const overview = useUi((s) => s.overview);
  const drawerOpen = useUi((s) => s.drawerOpen);
  const setDrawer = useUi((s) => s.setDrawer);
  const membersDrawerOpen = useUi((s) => s.membersDrawerOpen);
  const setMembersDrawer = useUi((s) => s.setMembersDrawer);
  const members = useMembersPanel();
  const showApp = phase === "online" || phase === "reconnecting";
  const stageOpen = useScreen((s) => s.stage !== null);
  const chatHidden = useScreen((s) => s.chatHidden);

  useTheme();
  useShortcuts();
  useEffect(() => controller.init(), []);
  useEffect(() => void accountActions.load(), []);
  useEffect(() => initDesktop(), []);
  useEffect(() => initUpdates(), []);

  useEffect(() => {
    document.title = `${unread > 0 ? `(${unread}) ` : ""}${showApp && serverName ? `${serverName} · ` : ""}Gwar`;
  }, [unread, serverName, showApp]);

  // Another server opens on its chat, as before; the overview is a place to go, not a landing page.
  useEffect(() => useUi.getState().showChat(), [address]);

  useEffect(() => {
    if (!isMobile) {
      setDrawer(false);
      setMembersDrawer(false);
    }
  }, [isMobile, setDrawer, setMembersDrawer]);

  return (
    <TooltipProvider>
      <div className="flex h-dvh w-full flex-col overflow-hidden bg-rail">
        <ConnectionWarning phase={phase} insecure={insecureConnection} />
        {showApp && <SharingBar />}
        <div className="flex min-h-0 w-full flex-1 overflow-hidden">
          {!isMobile && <SectionRail />}
          {/* Home and the overview lie over the server view instead of replacing it: the chat keeps
              its drafts, files and scroll position, and a watched share keeps playing underneath. */}
          <div className="relative flex min-w-0 flex-1">
            <div inert={home} className={cn("flex min-w-0 flex-1", home && "invisible")}>
              {!isMobile && <ServerRail />}
              {showApp ? (
                <div className={cn("flex min-w-0 flex-1", !isMobile && "gap-2 py-2 pr-2")}>
                  {!isMobile && <Sidebar card />}
                  {stageOpen ? (
                    <>
                      {/* Watching: the stage is the main content, the chat collapsible beside it. */}
                      <Stage card={!isMobile} />
                      {!isMobile && !chatHidden && (
                        <div className="flex w-[min(380px,32vw)] min-w-0 shrink-0">
                          <ChatArea card />
                        </div>
                      )}
                    </>
                  ) : (
                    <div className="relative flex min-w-0 flex-1">
                      <div inert={overview} className={cn("flex min-w-0 flex-1", !isMobile && "gap-2", overview && "invisible")}>
                        <ChatArea card={!isMobile} />
                        {!isMobile && members.open && <MemberList card />}
                      </div>
                      {overview && (
                        <div className="absolute inset-0 flex">
                          <Overview card={!isMobile} />
                        </div>
                      )}
                    </div>
                  )}
                </div>
              ) : (
                <ConnectScreen />
              )}
            </div>
            {home && (
              <div className={cn("absolute inset-0 flex", !isMobile && "p-2")}>
                <Home card={!isMobile} />
              </div>
            )}
          </div>
        </div>
      </div>
      {isMobile && (
        <DialogPrimitive.Root open={drawerOpen} onOpenChange={setDrawer}>
          <DialogPrimitive.Portal>
            <DialogPrimitive.Overlay className="anim-fade fixed inset-0 z-40 bg-black/55" />
            <DialogPrimitive.Content
              aria-describedby={undefined}
              onOpenAutoFocus={(e) => {
                // Do not focus (and tooltip) the first server button.
                e.preventDefault();
                (e.currentTarget as HTMLElement).focus();
              }}
              className="anim-drawer fixed inset-y-0 left-0 z-40 flex max-w-[92vw] flex-col bg-rail outline-none"
            >
              <DialogPrimitive.Title className="sr-only">{t("nav.channels")}</DialogPrimitive.Title>
              <div className="flex min-h-0 flex-1">
                <ServerRail />
                {showApp && <Sidebar className="w-[min(280px,calc(92vw-64px))]" />}
              </div>
              {/* Phones have no global rail: its sections are tabs here. */}
              <SectionTabs />
            </DialogPrimitive.Content>
          </DialogPrimitive.Portal>
        </DialogPrimitive.Root>
      )}
      {isMobile && showApp && (
        <DialogPrimitive.Root open={membersDrawerOpen} onOpenChange={setMembersDrawer}>
          <DialogPrimitive.Portal>
            <DialogPrimitive.Overlay className="anim-fade fixed inset-0 z-40 bg-black/55" />
            <DialogPrimitive.Content
              aria-describedby={undefined}
              className="anim-drawer-right fixed inset-y-0 right-0 z-40 flex max-w-[88vw] bg-side outline-none"
            >
              <DialogPrimitive.Title className="sr-only">{t("chat.members")}</DialogPrimitive.Title>
              <MemberList className="w-72 max-w-[88vw]" />
            </DialogPrimitive.Content>
          </DialogPrimitive.Portal>
        </DialogPrimitive.Root>
      )}
      <Dialogs />
      {showApp && <SharePresetDialog />}
      <Toaster />
    </TooltipProvider>
  );
}
