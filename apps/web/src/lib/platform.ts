import type { Platform } from "../proto/Platform";

/** Short client label shown next to people ("TS3" marks TeamSpeak users). */
export const PLATFORM_LABEL: Record<Platform, string> = { web: "Web", desktop: "App", mobile: "Mobile", ts3: "TS3", ts6: "TS6" };

export const isTeamSpeak = (platform: Platform | null | undefined): boolean => platform === "ts3" || platform === "ts6";
