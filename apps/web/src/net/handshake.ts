import { ed25519 } from "@noble/curves/ed25519.js";
import type { DeviceCertificate } from "../proto/DeviceCertificate";
import type { Hello } from "../proto/Hello";
import { decodeBase64Url, encodeBase64Url } from "./base64url";

/** Mirrors vc_proto::normalize_endpoint. The path is part of the binding. */
export function normalizeEndpoint(address: string): string {
  if (!address || /[\s\\?#]/u.test(address) || !/^[a-z]+:\/\//i.test(address)) throw new Error("invalid endpoint");
  const url = new URL(address);
  if (!["ws:", "wss:"].includes(url.protocol) || address.split("://")[1]?.split("/")[0]?.includes("@") || url.port === "0") {
    throw new Error("invalid endpoint");
  }
  url.hostname = url.hostname.toLowerCase().replace(/\.+$/, "");
  if (!url.hostname) throw new Error("invalid endpoint");
  return url.toString();
}

export function decodeFixed(value: unknown, length: number): Uint8Array {
  if (typeof value !== "string" || !/^[A-Za-z0-9_-]+$/.test(value)) throw new Error("invalid base64url");
  const bytes = decodeBase64Url(value);
  if (bytes.length !== length || encodeBase64Url(bytes) !== value) throw new Error("invalid base64url length or encoding");
  return bytes;
}

// Seed-generated Ed25519 keys and signatures have canonical prime-order points.
// This makes verification independent of permissive WebCrypto implementations.
function strictPoint(bytes: Uint8Array): void {
  const point = ed25519.Point.fromBytes(bytes, false);
  if (point.isSmallOrder() || !point.isTorsionFree()) throw new Error("invalid Ed25519 point");
}

export function serverPublicKey(value: unknown): Uint8Array {
  const bytes = decodeFixed(value, 32);
  strictPoint(bytes);
  return bytes;
}

export async function hashStatement(statement: string): Promise<string> {
  return encodeBase64Url(new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(statement))));
}

export async function certificateHash(c?: DeviceCertificate): Promise<string> {
  if (!c) return "-";
  decodeFixed(c.account_key, 32);
  decodeFixed(c.device_key, 32);
  decodeFixed(c.signature, 64);
  if (!Number.isSafeInteger(c.issued_at) || c.issued_at < 0 || !Number.isSafeInteger(c.expires_at) || c.expires_at <= c.issued_at) {
    throw new Error("invalid certificate timestamps");
  }
  return hashStatement(`gwar device v1\n${c.account_key}\n${c.device_key}\n${c.issued_at}\n${c.expires_at}\n${c.signature}`);
}

export function helloStatement(h: Pick<Hello, "endpoint" | "expected_server_pk" | "server_nonce" | "client_nonce" | "client_signing_pk" | "certificate_hash">): string {
  if (normalizeEndpoint(h.endpoint) !== h.endpoint) throw new Error("noncanonical endpoint");
  for (const value of [h.expected_server_pk, h.server_nonce, h.client_nonce, h.client_signing_pk]) decodeFixed(value, 32);
  if (h.certificate_hash !== "-") decodeFixed(h.certificate_hash, 32);
  return `gwar client hello v3\n${h.endpoint}\n${h.expected_server_pk}\n${h.server_nonce}\n${h.client_nonce}\n${h.client_signing_pk}\n${h.certificate_hash}`;
}

export async function proofStatement(h: Hello): Promise<string> {
  decodeFixed(h.signature, 64);
  if (await certificateHash(h.certificate ?? undefined) !== h.certificate_hash) throw new Error("certificate commitment mismatch");
  return `gwar server proof v3\n${await hashStatement(helloStatement(h))}\n${h.signature}`;
}

export async function verifyProof(h: Hello, value: unknown): Promise<void> {
  if (!value || typeof value !== "object" || Object.keys(value).length !== 1 || !("server_proof" in value)) throw new Error("missing server proof");
  const signature = decodeFixed(value.server_proof, 64);
  const key = serverPublicKey(h.expected_server_pk);
  strictPoint(signature.subarray(0, 32));
  const statement = new TextEncoder().encode(await proofStatement(h));
  if (!ed25519.verify(signature, statement, key, { zip215: false })) throw new Error("invalid server proof");
}

export function newNonce(): string {
  return encodeBase64Url(crypto.getRandomValues(new Uint8Array(32)));
}
