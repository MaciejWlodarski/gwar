import { expect, it } from "vitest";
import { contentSecurityPolicy } from "./csp";
it("keeps production scripts local and relaxes native transport only", () => {
  const web = contentSecurityPolicy();
  for (const rule of ["default-src 'self'", "script-src 'self' 'wasm-unsafe-eval'", "object-src 'none'", "frame-src 'none'", "base-uri 'none'", "form-action 'none'", "connect-src 'self' https: wss:"]) expect(web).toContain(rule);
  expect(web).not.toContain("'unsafe-eval'");
  expect(web).not.toContain(" http:");
  expect(web).not.toContain(" ws:");
  expect(contentSecurityPolicy(true)).toContain("ipc: http://ipc.localhost");
});
