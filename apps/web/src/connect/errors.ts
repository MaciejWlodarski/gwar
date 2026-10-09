import { tNow } from "../i18n";
import { ConnectApiError } from "./api";

/** Where the failure happened, so "unauthorized" can say the right thing. */
export type AccountContext = "signin" | "recover" | "password" | "session" | "register";

export function describeAccountError(e: unknown, context: AccountContext = "signin"): string {
  if (!(e instanceof ConnectApiError)) return tNow("account.err.other", { message: e instanceof Error ? e.message : String(e) });
  switch (e.kind) {
    case "network":
      return tNow("account.err.network");
    case "rate_limited":
      return tNow("account.err.rate");
    case "taken":
      return tNow("account.err.taken");
    case "revoked":
      return tNow("account.err.revoked");
    case "unauthorized":
      return tNow(
        context === "recover" ? "account.err.code" : context === "password" ? "account.err.passwordOnly" : context === "session" ? "account.err.session" : "account.err.password",
      );
    case "not_found":
      return tNow(context === "recover" ? "account.err.code" : "account.err.password");
    default:
      return tNow("account.err.other", { message: e.message });
  }
}
