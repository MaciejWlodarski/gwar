import { pageContextFromLocation, parseServerAddress, parseTeamSpeakAddress, type PageContext } from "../net/address";
import { serverPins, type PinStore } from "../net/server-pins";

export interface ServerRef {
  kind?: "vc" | "teamspeak";
  address: string;
  insecureLan?: boolean;
}

function currentPage(): PageContext {
  return typeof location === "undefined" ? { protocol: "https:", hostname: "", port: "" } : pageContextFromLocation(location);
}

/**
 * Which server an address means, so one server is saved once however it was typed: `gwar.app`,
 * `GWAR.app` and `wss://gwar.app/ws` (an invite link) are the same server. A vc server this device
 * has connected to is known by its pinned identity key, so other addresses of it (an IP address,
 * another name) match too; otherwise by the WebSocket URL the address opens. A TeamSpeak server is
 * compared by `host:port`. What does not parse compares as typed (trimmed, lower case).
 */
export function serverKey(server: ServerRef, page: PageContext = currentPage(), pins: PinStore | null = storedPins()): string {
  const kind = server.kind ?? "vc";
  const typed = server.address.trim().toLowerCase();
  if (kind === "teamspeak") {
    const ts = parseTeamSpeakAddress(server.address);
    return `teamspeak|${ts.ok ? ts.value.address : typed}`;
  }
  // A remote plaintext address only parses with the LAN opt-in: it still names one server.
  const vc = parseServerAddress(server.address, page, !!server.insecureLan);
  const lan = vc.ok ? vc : parseServerAddress(server.address, { ...page, protocol: "tauri:" }, true);
  if (!lan.ok) return `vc|${typed}`;
  const pin = pinned(pins, lan.value.url);
  return pin ? `vc-key|${pin}` : `vc|${lan.value.url}`;
}

export function sameServer(a: ServerRef, b: ServerRef): boolean {
  return serverKey(a) === serverKey(b);
}

function storedPins(): PinStore | null {
  return typeof localStorage === "undefined" ? null : serverPins;
}

function pinned(pins: PinStore | null, endpoint: string): string | undefined {
  try {
    return pins?.load(endpoint)?.server_pk;
  } catch {
    return undefined; // a damaged pin: the address alone decides
  }
}
