#!/usr/bin/env python3
"""Compare local clients through a temporary remote Gwar server and SSH signaling tunnel."""

import argparse
import ipaddress
import re
import shlex
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import PurePosixPath

from run_local import Results, add_arguments, environment, install_signals, run_client, stop_process


def port(value):
    number = int(value)
    if not 1 <= number <= 65535:
        raise argparse.ArgumentTypeError("port must be between 1 and 65535")
    return number


def ip(value):
    try:
        return ipaddress.ip_address(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError(str(error)) from error


def remote_variant(value):
    name, separator, binary = value.partition("=")
    if not separator or not binary or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]*", name):
        raise argparse.ArgumentTypeError("expected name=path/to/remote-server")
    return name, binary


def address(host, number):
    return f"[{host}]:{number}" if host.version == 6 else f"{host}:{number}"


def ssh(args, command, check=True):
    result = subprocess.run(
        ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=20", "--", args.ssh_host, command],
        stdin=subprocess.DEVNULL, text=True, capture_output=True, timeout=45, check=False,
    )
    if check and result.returncode:
        raise RuntimeError(f"SSH command failed ({result.returncode}): {result.stderr.strip()}")
    return result


def start_server(args, directory, server_binary):
    checkout = shlex.quote(args.remote_checkout)
    server = PurePosixPath(server_binary)
    if not server.is_absolute():
        server = PurePosixPath(args.remote_checkout) / server
    command = shlex.join([
        "nohup", str(server), "--data-dir", f"{directory}/data",
        "--http", f"127.0.0.1:{args.http_port}", "--media", address(args.media_bind, args.media_port),
        "--public-ip", str(args.public_ip), "--max-clients", "8", "--no-connect", "--insecure-any-host",
    ])
    # VC_* can enable unrelated services. Extract variable names only, then unset them.
    # ';' keeps `cd` out of the background job. All three streams must be detached,
    # including stdin: without '< /dev/null' a remote start has hung in practice.
    script = (
        f"cd {checkout} || exit 1; "
        r"""for variable in $(env | sed -n 's/^\(VC_[A-Za-z0-9_]*\)=.*/\1/p'); do unset "$variable"; done; """
        f"{command} > {shlex.quote(directory + '/server.log')} 2>&1 < /dev/null & "
        f"pid=$!; printf '%s\\n' \"$pid\" > {shlex.quote(directory + '/pid')}; printf '%s\\n' \"$pid\""
    )
    result = ssh(args, script)
    return int(result.stdout.strip())


def stop_server(args, directory):
    # The pidfile also allows cleanup when the start reply was lost. It lives in
    # this run's exclusively created directory; no process-name lookup is used.
    pidfile = shlex.quote(directory + "/pid")
    script = (
        f"if test -f {pidfile}; then pid=$(cat {pidfile}); "
        "case $pid in ''|*[!0-9]*) exit 1;; esac; test \"$pid\" -gt 1 || exit 1; "
        "if kill -0 \"$pid\" 2>/dev/null; then kill \"$pid\" || exit 1; "
        "attempt=0; while kill -0 \"$pid\" 2>/dev/null && test \"$attempt\" -lt 5; do "
        "sleep 1; attempt=$((attempt + 1)); done; "
        "if kill -0 \"$pid\" 2>/dev/null; then kill -9 \"$pid\" || exit 1; fi; fi; fi"
    )
    ssh(args, script)
    ssh(args, f"rm -rf -- {shlex.quote(directory)}")


def wait_ready(args, tunnel, directory, pid):
    url = f"http://127.0.0.1:{args.tunnel_port}/health"
    # Do not let proxy environment settings redirect the loopback readiness check.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        if tunnel.poll() is not None:
            raise RuntimeError("SSH tunnel exited; inspect tunnel.txt")
        try:
            with opener.open(url, timeout=1) as response:
                if response.status == 200 and response.read() == b"ok":
                    # Ensure a pre-existing listener did not answer after our server
                    # failed to bind. Verify this server's ready message and PID.
                    ready = ssh(args, f"kill -0 {pid} 2>/dev/null && grep -q 'server ready' {shlex.quote(directory + '/server.log')}", check=False)
                    if ready.returncode == 0:
                        return
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(0.2)
    raise RuntimeError("temporary server did not become ready within 20 seconds; inspect its server log")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    add_arguments(parser)
    parser.add_argument("--ssh-host", required=True, help="SSH destination, including user or configured host alias")
    parser.add_argument("--public-ip", type=ip, required=True, help="server IP reachable by the local client for UDP")
    parser.add_argument("--media-bind", type=ip, required=True, help="remote local interface to bind for UDP")
    parser.add_argument("--media-port", type=port, required=True)
    parser.add_argument("--http-port", type=port, required=True, help="remote loopback HTTP port")
    parser.add_argument("--tunnel-port", type=port, required=True, help="local loopback signaling port")
    parser.add_argument("--remote-checkout", required=True, help="absolute checkout path on the remote host")
    servers = parser.add_mutually_exclusive_group(required=True)
    servers.add_argument("--server-binary", help="one remote server binary for all variants, absolute or checkout-relative")
    servers.add_argument("--server-variant", type=remote_variant, action="append", help="name=remote-server-binary; repeat for every client variant")
    args = parser.parse_args()
    if args.ssh_host.startswith("-") or not args.ssh_host.strip():
        parser.error("--ssh-host must be an SSH destination")
    if not PurePosixPath(args.remote_checkout).is_absolute():
        parser.error("--remote-checkout must be absolute (expand ~ yourself)")
    names = [name for name, _ in args.variant]
    server_binaries = dict(args.server_variant or [])
    if args.server_variant:
        if len(server_binaries) != len(args.server_variant) or set(server_binaries) != set(names):
            parser.error("--server-variant must name each client variant exactly once")
    else:
        server_binaries = {name: args.server_binary for name in names}
    install_signals()
    results = Results(
        args, "remote", ssh_host=args.ssh_host, public_ip=str(args.public_ip), media_bind=str(args.media_bind),
        media_port=args.media_port, http_port=args.http_port, tunnel_port=args.tunnel_port,
        remote_checkout=args.remote_checkout, server_binaries=server_binaries,
    )
    try:
        with (results.out / "tunnel.txt").open("x") as log:
            tunnel = subprocess.Popen([
                "ssh", "-N", "-o", "BatchMode=yes", "-o", "ConnectTimeout=20", "-o", "ExitOnForwardFailure=yes",
                "-L", f"127.0.0.1:{args.tunnel_port}:127.0.0.1:{args.http_port}", "--", args.ssh_host,
            ], stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)
            try:
                for run, name in enumerate(results.order, 1):
                    # Create a new directory beneath the provided checkout, never
                    # delete or reuse a caller's data directory.
                    template = str(PurePosixPath(args.remote_checkout) / "target" / "latency-server.XXXXXXXX")
                    directory = ssh(args, f"mkdir -p -- {shlex.quote(str(PurePosixPath(template).parent))}; mktemp -d {shlex.quote(template)}").stdout.strip()
                    if not directory.startswith(template.rsplit(".", 1)[0] + ".") or "\n" in directory:
                        raise RuntimeError(f"unexpected temporary directory reply: {directory!r}")
                    try:
                        pid = start_server(args, directory, server_binaries[name])
                        if pid <= 1:
                            raise RuntimeError("invalid server PID")
                        wait_ready(args, tunnel, directory, pid)
                        print(f"run {run}/{len(results.order)}: {name} (server pid {pid})", flush=True)
                        env = environment()
                        env.update(LATENCY_URL=f"ws://127.0.0.1:{args.tunnel_port}/ws", LATENCY_SERVER=address(args.public_ip, args.media_port))
                        client_pid = run_client(results.binaries[name], results.path(run, name), args.markers, env)
                        results.accept(run, name, client_pid, server_pid=pid, server_binary=server_binaries[name])
                    finally:
                        try:
                            log_text = ssh(args, f"cat -- {shlex.quote(directory + '/server.log')}", check=False).stdout
                            (results.out / f"run-{run:02d}-{name}-server.txt").write_text(log_text)
                        finally:
                            stop_server(args, directory)
                results.finish()
            finally:
                stop_process(tunnel)
    except BaseException as error:
        results.fail(error)
        raise


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        raise SystemExit(str(error) or "interrupted") from error
