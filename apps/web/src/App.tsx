import * as DialogPrimitive from "@radix-ui/react-dialog";
import { useEffect } from "react";
import { useT } from "./i18n";
import { useIsMobile } from "./lib/media";
import { initDesktop } from "./platform/desktop";
import { controller } from "./state/controller";
import { totalDmUnread } from "./state/reducer";
import { useSession, useUi } from "./state/stores";
import { ChatArea } from "./ui/Chat";
import { ConnectScreen } from "./ui/ConnectScreen";
import { Dialogs } from "./ui/Dialogs";
import { useShortcuts, useTheme } from "./ui/hooks";
import { TooltipProvider } from "./ui/kit";
import { MemberList } from "./ui/MemberList";
import { useMembersPanel } from "./ui/members";
import { ServerRail } from "./ui/ServerRail";
import { Sidebar } from "./ui/Sidebar";
import { Toaster } from "./ui/Toaster";

export function App() {
  const t = useT();
  const isMobile = useIsMobile();
  const phase = useSession((s) => s.phase);
  const serverName = useSession((s) => s.server?.name);
  const unread = useSession((s) => {
    const dm = totalDmUnread(s);
    const ch = Object.entries(s.threads).reduce((n, [k, th]) => (k === "server" || k.startsWith("ch:") ? n + th.unread : n), 0);
    return dm + ch;
  });
  const drawerOpen = useUi((s) => s.drawerOpen);
  const setDrawer = useUi((s) => s.setDrawer);
  const membersDrawerOpen = useUi((s) => s.membersDrawerOpen);
  const setMembersDrawer = useUi((s) => s.setMembersDrawer);
  const members = useMembersPanel();
  const showApp = phase === "online" || phase === "reconnecting";

  useTheme();
  useShortcuts();
  useEffect(() => controller.init(), []);
  useEffect(() => initDesktop(), []);

  useEffect(() => {
    document.title = `${unread > 0 ? `(${unread}) ` : ""}${showApp && serverName ? `${serverName} · ` : ""}Gwar`;
  }, [unread, serverName, showApp]);

  useEffect(() => {
    if (!isMobile) {
      setDrawer(false);
      setMembersDrawer(false);
    }
  }, [isMobile, setDrawer, setMembersDrawer]);

  return (
    <TooltipProvider>
      <div className="flex h-dvh w-full overflow-hidden bg-rail">
        {!isMobile && <ServerRail />}
        {showApp ? (
          <>
            {!isMobile && <Sidebar />}
            <ChatArea />
            {!isMobile && members.open && <MemberList />}
          </>
        ) : (
          <ConnectScreen />
        )}
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
              className="anim-drawer fixed inset-y-0 left-0 z-40 flex max-w-[92vw] bg-rail outline-none"
            >
              <DialogPrimitive.Title className="sr-only">{t("nav.channels")}</DialogPrimitive.Title>
              <ServerRail />
              {showApp && <Sidebar className="w-[min(280px,calc(92vw-72px))]" />}
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
      <Toaster />
    </TooltipProvider>
  );
}
