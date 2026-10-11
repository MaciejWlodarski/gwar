import { History, Loader2, ShieldX, TriangleAlert } from "lucide-react";
import { useT, type TFn } from "../i18n";
import { fullyVerified, type MessageVerification } from "../net/message-signature";
import { TeamSpeakBadge } from "./badges";
import { Tooltip } from "./kit";

export function verificationExplanation(v: MessageVerification, t: TFn): string {
  if (v.status === "unsigned-ts") return t("chat.teamSpeakUnsigned");
  if (v.status === "unsigned") return t("chat.notVerified");
  if (v.status === "invalid") return t(v.continuity === "rollback" ? "chat.rollback" : v.continuity === "fork" ? "chat.fork" : "chat.signatureInvalid");
  const notes = v.authorization.map((a) => t(a === "device-revoked" ? "chat.deviceRevoked" : a === "certificate-expired" ? "chat.certificateExpired" : a === "channel-reused" ? "chat.channelReused" : a === "channel-history-incomplete" ? "chat.channelHistoryIncomplete" : "chat.revocationsStale"));
  if (v.continuity !== "proven") notes.push(t(v.continuity === "storage-unavailable" ? "chat.checkpointsUnavailable" : "chat.continuityUnproven"));
  return [t("chat.signatureValid"), ...notes].join("; ");
}

/** Until its signature is checked a message is visibly pending, so quiet always means verified. */
export function MessageVerificationPending() {
  const t = useT();
  const label = t("chat.verifying");
  return (
    <Tooltip label={label}>
      <span tabIndex={0} aria-label={label} data-testid="message-verification-pending" className="inline-flex cursor-default items-center align-middle text-subtle">
        <Loader2 aria-hidden className="size-3.5 animate-spin" />
      </span>
    </Tooltip>
  );
}

/** Positive verification stays quiet; historical qualifications stay calm. */
export function MessageVerificationIndicator({ verification, showAuthor = false }: { verification?: MessageVerification; showAuthor?: boolean }) {
  const t = useT();
  if (!verification || fullyVerified(verification)) return null;
  const label = verificationExplanation(verification, t);
  if (verification.status === "unsigned-ts") return showAuthor ? (
    <Tooltip label={label}><span tabIndex={0} aria-label={label} data-testid="message-verification-teamspeak"><TeamSpeakBadge short /></span></Tooltip>
  ) : null;
  const invalid = verification.status === "invalid", unsigned = verification.status === "unsigned";
  const Icon = invalid ? ShieldX : unsigned ? TriangleAlert : History;
  return (
    <Tooltip label={label}>
      <span tabIndex={0} aria-label={label} data-message-warning={invalid || unsigned || undefined}
        data-testid={`message-verification-${invalid ? "invalid" : unsigned ? "unsigned" : "qualified"}`}
        className={`inline-flex cursor-default items-center gap-1 align-middle text-[11px] ${invalid ? "text-danger" : unsigned ? "text-warn" : "text-subtle"}`}>
        <Icon aria-hidden className="size-3.5 shrink-0" />
        {invalid && <span>{t("chat.invalid")}</span>}
      </span>
    </Tooltip>
  );
}
