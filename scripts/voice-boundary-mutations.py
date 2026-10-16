"""Temporary CI-only mutation driver; never starts TeamSpeak on the developer Mac."""
from pathlib import Path
import subprocess
import sys

media = Path("crates/vc-server/src/media/mod.rs")
listeners = Path("crates/vc-server/src/teamspeak/listeners.rs")
probe = Path("crates/vc-server/tests/teamspeak_channels.rs")
originals = {path: path.read_text() for path in (media, listeners, probe)}
command = ["cargo", "test", "-p", "vc-server", "--features", "test-hooks", "--test", "teamspeak_channels", "--", "--ignored", "--test-threads=1", "--nocapture"]
for name, expected in [
    ("source_voice", "forbidden TS admission, marker=52"),
    ("source_whisper", "forbidden TS admission, marker=53"),
    ("listener_generation", "queued text survived its listener connection"),
    ("command_barrier", "official probe command acknowledgement"),
]:
    try:
        if name.startswith("source_"):
            changed = originals[media].replace("!origin.encrypted\n            && self.current", "self.current")
            assert changed != originals[media]
            media.write_text(changed)
            if name == "source_whisper":
                # Exercise whisper independently instead of stopping at the voice assertion.
                line = "    ingress_attempt(probe, &shared, client.welcome.session, ts_clid, clid, false, 52).await;\n"
                assert line in originals[probe]
                probe.write_text(originals[probe].replace(line, ""))
        elif name == "command_barrier":
            changed = originals[probe].replace('"clientupdate",\n                        );', '"clientgetconnectioninfo",\n                        );').replace('cmd.write_arg("client_input_muted", &0);', 'cmd.write_arg("clid", &con.get_state().unwrap().own_client.0);')
            assert changed != originals[probe]
            probe.write_text(changed)
        else:
            changed = originals[listeners].replace(" && l.connection.load(Ordering::Acquire) == text.connection", "")
            assert changed != originals[listeners]
            listeners.write_text(changed)
        result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=300)
        print(result.stdout, flush=True)
        assert result.returncode == 101 and "test result: FAILED" in result.stdout and expected in result.stdout, f"mutation {name} did not fail at its specific boundary"
        print(f"OFFICIAL MUTATION KILLED: {name}: {expected}", flush=True)
    finally:
        for path, original in originals.items():
            path.write_text(original)
