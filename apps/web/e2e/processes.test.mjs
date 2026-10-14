import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { test } from "node:test";
import { stopProcesses } from "./processes.mjs";

test("escalates after the group leader exits while a descendant ignores SIGTERM", async () => {
  const child = spawn(process.execPath, ["-e", `
    const { spawn } = require("node:child_process");
    const descendant = spawn(process.execPath, ["-e", 'process.on("SIGTERM", () => {}); console.log("ready"); setInterval(() => {}, 1000);'], { stdio: ["ignore", "pipe", "ignore"] });
    descendant.stdout.once("data", () => { console.log(descendant.pid); process.exit(0); });
  `], { detached: true, stdio: ["ignore", "pipe", "inherit"] });
  let output = "";
  child.stdout.on("data", chunk => { output += chunk; });
  try {
    await once(child, "exit");
    assert.equal(child.exitCode, 0);
    const descendant = Number(output.trim());
    assert.ok(descendant > 0);
    process.kill(descendant, 0);
    await stopProcesses([child], 100);
    assert.throws(() => process.kill(-child.pid, 0), { code: "ESRCH" });
  } finally {
    await stopProcesses([child], 100);
  }
});
