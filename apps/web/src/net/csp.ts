/** Production web and native policies; Vite dev omits the meta policy. */
export function contentSecurityPolicy(desktop = false): string {
  const connect = desktop ? "'self' ipc: http://ipc.localhost https: wss: http: ws:" : "'self' https: wss:";
  return [
    "default-src 'self'", "script-src 'self' 'wasm-unsafe-eval'", "style-src 'self' 'unsafe-inline'",
    "object-src 'none'", "base-uri 'none'", "img-src 'self' blob: https:",
    "media-src 'self' blob: https:", "font-src 'self'", `connect-src ${connect}`, "form-action 'none'",
  ].join("; ");
}
