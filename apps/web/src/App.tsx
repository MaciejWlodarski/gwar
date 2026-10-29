import { useEffect } from "react";
import { accountActions } from "./state/account";
import { cn } from "./lib/cn";
import { useIsMobile } from "./lib/media";
import { initDesktop } from "./platform/desktop";
import { initUpdates } from "./platform/updater";
import { controller } from "./state/controller";
import { useConnectUi, useSession, useUi, type ServerPane } from "./state/stores";
import { BugReportSection } from "./ui/BugReport";
import { ChatArea } from "./ui/Chat";
import { Connecting } from "./ui/Connecting";
import { ConnectionWarning } from "./ui/ConnectionWarning";
import { ConnectScreen } from "./ui/ConnectScreen";
import { Dialogs } from "./ui/Dialogs";
import { useShortcuts, useTheme } from "./ui/hooks";
import { TooltipProvider } from "./ui/kit";
import { MemberList } from "./ui/MemberList";
import { useMembersPanel } from "./ui/members";
import { Home } from "./ui/Home";
import { Overview } from "./ui/Overview";
import { MoreSheet, TabBar } from "./ui/MobileNav";
import { paneBack, useBackHistory, useServerUnread } from "./ui/nav";
import { SectionRail } from "./ui/SectionRail";
import { ServerRail } from "./ui/ServerRail";
import { SettingsSection } from "./ui/Settings";
import { SharePresetDialog, SharingBar, Stage } from "./ui/Screen";
import { useScreen } from "./state/screen";
import { Sidebar } from "./ui/Sidebar";
import { IdentityNotices } from "./ui/IdentityNotices";
import { Toaster } from "./ui/Toaster";

export function App() {
  const isMobile = useIsMobile();
  const insecureConnection = useSession((s) => s.insecureConnection);
  const phase = useSession((s) => s.phase);
  const serverName = useSession((s) => s.server?.name);
  const unread = useServerUnread();
  const section = useUi((s) => s.section);
  const covered = section !== "servers";
  // With a share on stage the stage wins; the overview waits until it closes.
  const overview = useUi((s) => s.overview);
  const pane = useUi((s) => s.pane);
  const members = useMembersPanel();
  const restoring = useConnectUi((s) => s.restoring);
  const connecting = restoring;
  const showApp = connecting || phase === "online" || phase === "reconnecting";
  const stageOpen = useScreen((s) => s.stage !== null);
  const chatHidden = useScreen((s) => s.chatHidden);

  useTheme();
  useShortcuts();
  useEffect(() => controller.init(), []);
  useEffect(() => { void accountActions.load().then(() => controller.restoreSession()); }, []);
  useEffect(() => initDesktop(), []);
  useEffect(() => initUpdates(), []);

  useEffect(() => {
    document.title = `${unread > 0 ? `(${unread}) ` : ""}${showApp && serverName ? `${serverName} · ` : ""}Gwar`;
  }, [unread, serverName, showApp]);

  useEffect(() => {
    if (!isMobile) useUi.getState().setMore(false);
  }, [isMobile]);
  useBackHistory(isMobile);

  // Phones show one page of the server section at a time. The conversation and its members fill the
  // screen without the tab bar, leaving room for the keyboard.
  const paneShown = (p: ServerPane) => !isMobile || pane === p;
  const fullScreen = isMobile && !covered && showApp && pane !== "list";

  return (
    <TooltipProvider>
      <div className="flex h-dvh w-full flex-col overflow-hidden bg-rail">
        <ConnectionWarning phase={phase} insecure={insecureConnection} />
        <IdentityNotices />
        {showApp && <SharingBar />}
        <div className="flex min-h-0 w-full flex-1 overflow-hidden">
          {!isMobile && <SectionRail />}
          {/* The other sections and the overview lie over the server view instead of replacing it: the chat
              keeps its drafts, files and scroll position, and voice and a watched share keep running underneath. */}
          <div className="relative flex min-w-0 flex-1">
            <div inert={covered} className={cn("flex min-w-0 flex-1", !isMobile && "gap-2 py-2 pr-2", covered && "invisible")}>
              {(!isMobile || !showApp) && <ServerRail card={!isMobile} />}
              {showApp ? (
                <div className={cn("relative flex min-w-0 flex-1", !isMobile && "gap-2")}>
                  {!isMobile && <Sidebar card connecting={connecting} />}
                  <div inert={!paneShown("chat")} className={cn("flex min-w-0 flex-1", !isMobile && "gap-2", !paneShown("chat") && "invisible")}>
                    {connecting ? <Connecting card={!isMobile} /> : stageOpen ? (
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
                  {isMobile && pane === "list" && (
                    <div className="absolute inset-0 flex">
                      <ServerRail />
                      {connecting ? <Connecting /> : <Sidebar fill />}
                    </div>
                  )}
                  {isMobile && pane === "members" && (
                    <div className="absolute inset-0 flex">
                      <MemberList onBack={() => paneBack("chat")} />
                    </div>
                  )}
                </div>
              ) : (
                <ConnectScreen card={!isMobile} />
              )}
            </div>
            {covered && (
              <div className={cn("absolute inset-0 flex", !isMobile && "py-2 pr-2")}>
                {section === "home" ? (
                  <Home card={!isMobile} />
                ) : section === "settings" ? (
                  <SettingsSection card={!isMobile} />
                ) : (
                  <BugReportSection card={!isMobile} />
                )}
              </div>
            )}
          </div>
        </div>
        {isMobile && !fullScreen && <TabBar />}
      </div>
      {isMobile && <MoreSheet />}
      <Dialogs />
      {showApp && <SharePresetDialog />}
      <Toaster />
    </TooltipProvider>
  );
}
