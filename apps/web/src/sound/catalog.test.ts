import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { expect, it } from "vitest";
import score from "../../../../shared/sound-cues.json";
import { renderCue, type SoundCue } from "./catalog";

it("rejects a cue with a missing timbre instead of throwing or substituting another sound", () => {
  const timbres = score.timbres as Record<string, unknown>;
  const name = score.cues.mute.timbre, original = timbres[name];
  delete timbres[name];
  try { expect(renderCue("mute", 0.6)).toBeNull(); }
  finally { timbres[name] = original; }
});

it.each(["unknown", "toString", "__proto__"])("rejects unknown timbre %s", name => {
  const original = score.cues.mute.timbre;
  score.cues.mute.timbre = name;
  try { expect(renderCue("mute", 0.6)).toBeNull(); }
  finally { score.cues.mute.timbre = original; }
});

it.each(["unknown", "toString", "__proto__"])("rejects unknown cue name %s as the native renderer does", name => {
  expect(renderCue(name as SoundCue, 0.6)).toBeNull();
});

it.each(["valid", "missing", "unknown", "toString"])("the exporter validates timbres before writing WAVs (%s)", name => {
  const root = mkdtempSync(join(tmpdir(), "gwar-sound-score-"));
  try {
    mkdirSync(join(root, "scripts")); mkdirSync(join(root, "shared"));
    const script = join(root, "scripts/export-sound-cues.py"), output = join(root, "output");
    copyFileSync(new URL("../../../../scripts/export-sound-cues.py", import.meta.url), script);
    const fixture = structuredClone(score);
    if (name === "missing") delete (fixture.timbres as Record<string, unknown>)[fixture.cues.mute.timbre];
    else if (name !== "valid") fixture.cues.mute.timbre = name;
    writeFileSync(join(root, "shared/sound-cues.json"), JSON.stringify(fixture));
    const result = spawnSync("python3", [script, output], { encoding: "utf8" });
    if (name !== "valid") {
      expect(result.status).toBe(2);
      expect(result.stderr).toContain("unknown timbre");
      expect(result.stderr).not.toContain("Traceback");
      expect(existsSync(output)).toBe(false);
    } else {
      expect(result.status).toBe(0);
      expect(readFileSync(join(output, "README.txt"), "utf8")).toContain("mute.wav");
      expect(readFileSync(join(output, "mute.wav")).subarray(0, 4).toString()).toBe("RIFF");
    }
  } finally { rmSync(root, { recursive: true, force: true }); }
});
