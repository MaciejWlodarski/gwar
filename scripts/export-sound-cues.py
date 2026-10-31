#!/usr/bin/env python3
"""Render Gwar's original runtime score to 48 kHz mono PCM WAVs (stdlib only)."""
import argparse
import json
import math
from pathlib import Path
import struct
import wave

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("output", type=Path)
parser.add_argument("--volume", type=float, default=0.6, help="0..1, defaults to the app's 60%%")
args = parser.parse_args()
if not 0 <= args.volume <= 1:
    parser.error("volume must be between 0 and 1")
score = json.loads((Path(__file__).resolve().parent.parent / "shared/sound-cues.json").read_text())
timbres, cues = score["timbres"], score["cues"]
# Validate the whole score before creating any output, as the runtime rejects
# cues whose timbre is unavailable rather than substituting a different sound.
resolved_timbres = {}
for cue, spec in cues.items():
    timbre = timbres.get(spec.get("timbre"))
    if timbre is None:
        parser.error(f"cue {cue!r}: unknown timbre {spec.get('timbre')!r}")
    resolved_timbres[cue] = timbre
descriptions = dict(zip(cues, [
    "You joined voice", "You left voice or disconnected", "Someone joined your voice channel",
    "Someone left your voice channel", "Microphone muted", "Microphone unmuted", "Deafened",
    "Undeafened", "Voice connection lost", "Voice connection restored", "You were mentioned",
    "Incoming direct message", "Someone started sharing in your voice channel", "Your screen share stopped unexpectedly",
]))
args.output.mkdir(parents=True, exist_ok=True)
lines = ["Gwar sound cues", "", "Original synthesized plucks, one timbre per group; no samples or third-party sounds.",
         f"48 kHz, mono, 16-bit PCM. Volume: {args.volume:.0%} (app default: 60%).",
         "The web and native players synthesize the same score at runtime.", ""]
for cue, spec in cues.items():
    notes, timbre = spec["notes"], resolved_timbres[cue]
    samples = [0.0] * round(max(at + duration for _, at, duration in notes) * 48)
    for frequency, at, duration in notes:
        start, length = round(at * 48), round(duration * 48)
        for i in range(length):
            progress, phase = i / length, 2 * math.pi * frequency * i / 48000
            envelope = math.sin(math.pi * progress) ** 2 * math.exp(-timbre["decay"] * progress)
            tone = sum(amplitude * math.sin(multiple * phase) for multiple, amplitude in timbre["partials"])
            samples[start + i] += args.volume * 0.18 * timbre["level"] * envelope * tone
    with wave.open(str(args.output / f"{cue}.wav"), "wb") as wav:
        wav.setparams((1, 2, 48000, 0, "NONE", "not compressed"))
        wav.writeframes(b"".join(struct.pack("<h", round(max(-1, min(1, s)) * 32767)) for s in samples))
    lines.append(f"{cue}.wav - {descriptions[cue]} ({len(samples) / 48:.0f} ms)")
(args.output / "README.txt").write_text("\n".join(lines) + "\n")
print(f"Exported {len(cues)} WAVs and README.txt to {args.output}")
