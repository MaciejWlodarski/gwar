import type { ServerRef } from "../lib/server-key";
import { AtSign, Bell, BellOff } from "lucide-react";
import { useT } from "../i18n";
import { notificationMode, notificationServerKey } from "../lib/notification-scope";
import { useSettings, type NotificationMode } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import { Button, Dialog } from "./kit";

export function NotificationBadge({ channel, always = false, serverRef, className = "" }: { channel: number | null; always?: boolean; serverRef?: ServerRef; className?: string }) {
  const t = useT();
  const server = useSession(s => notificationServerKey(serverRef ?? s));
  const overrides = useSettings(s => s.notificationOverrides);
  const mode = notificationMode(overrides, server, channel);
  if (!mode && !always) return null;
  const Icon = mode === "none" ? BellOff : mode === "mentions" ? AtSign : Bell;
  return <span className={`inline-flex shrink-0 text-subtle ${className}`} title={t(mode ? `notify.mode.${mode}` : "notify.mode.default")}><Icon aria-hidden className="size-3.5" /></span>;
}

export function NotificationSettingsDialog({ channel, serverRef, name: suppliedName }: { channel: number | null; serverRef?: ServerRef; name?: string }) {
  const t = useT();
  const close = useUi(s => s.closeDialog);
  const session = useSession();
  const server = notificationServerKey(serverRef ?? session);
  const overrides = useSettings(s => s.notificationOverrides);
  const setMode = useSettings(s => s.setNotificationMode);
  const mode = channel === null ? overrides.servers[server] : overrides.channels[server]?.[channel];
  const name = suppliedName ?? (channel === null ? session.server?.name || session.address : session.channels[channel]?.name || "");
  return <Dialog open onOpenChange={o => !o && close()} title={t("notify.scopeTitle")} description={name} width="max-w-sm" footer={<Button onClick={close}>{t("common.close")}</Button>}>
    <fieldset className="flex flex-col gap-2">
      <legend className="mb-3 text-sm text-muted">{t("notify.scopeHint")}</legend>
      {(["default", "all", "mentions", "none"] as const).map(value => <label key={value} className="flex min-h-11 cursor-pointer items-center gap-3 rounded-lg border border-line px-3 py-2 text-sm hover:bg-hover">
        <input type="radio" name="notification-mode" value={value} checked={(mode ?? "default") === value} onChange={() => setMode(server, channel, value === "default" ? null : value as NotificationMode)} className="size-4 accent-accent" />
        {t(value === "default" && channel !== null ? "notify.mode.server" : `notify.mode.${value}`)}
      </label>)}
    </fieldset>
  </Dialog>;
}
