#!/usr/bin/env python3
"""Measure a real PTY session, without a local compiler or terminal renderer.

The benchmark drains terminal output but excludes the cost of Kitty rendering.
Linux schedstat measures CPU more precisely than /proc/stat's 10 ms ticks.
"""
import argparse
import fcntl
import json
import os
import pty
import select
import struct
import subprocess
import termios
import time


def counters(pid):
    root = f"/proc/{pid}/"
    with open(root + "schedstat") as f:
        cpu_ns = int(f.read().split()[0])
    with open(root + "io") as f:
        io = dict(line.strip().split(": ") for line in f)
    with open(root + "status") as f:
        status = dict(line.strip().split(":", 1) for line in f if ":" in line)
    return {
        "cpu_ns": cpu_ns,
        "read_calls": int(io["syscr"]),
        "write_calls": int(io["syscw"]),
        "voluntary_switches": int(status["voluntary_ctxt_switches"]),
        "rss_kib": int(status["VmRSS"].split()[0]),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary")
    parser.add_argument("--seconds", type=float, default=20)
    parser.add_argument("--columns", type=int, default=120)
    parser.add_argument("--rows", type=int, default=30)
    args = parser.parse_args()
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", args.rows, args.columns, 0, 0))
    # Avoid unrelated compositor traffic for a repeatable visible baseline.
    env = dict(os.environ)
    env.pop("NIRI_SOCKET", None)
    proc = subprocess.Popen([os.path.abspath(args.binary)], stdin=slave, stdout=slave, stderr=slave, env=env)
    try:
        # Establish the first frame before counting steady-state costs.
        until = time.monotonic() + 1.5
        while time.monotonic() < until:
            if select.select([master], [], [], 0.1)[0]:
                os.read(master, 1 << 20)
        initial = counters(proc.pid)
        start = time.monotonic()
        size = 0
        while time.monotonic() - start < args.seconds:
            if select.select([master], [], [], 0.1)[0]:
                size += len(os.read(master, 1 << 20))
        elapsed = time.monotonic() - start
        final = counters(proc.pid)
        report = {k: final[k] - initial[k] for k in initial if k != "rss_kib"}
        report.update(seconds=round(elapsed, 2), cpu_ms=round(report.pop("cpu_ns") / 1e6, 3),
                      terminal_bytes=size, bytes_per_second=round(size / elapsed), rss_kib=final["rss_kib"])
        os.write(master, b"q")
        assert proc.wait(timeout=5) == 0
        print(json.dumps(report, indent=2))
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()
        os.close(master)
        os.close(slave)


if __name__ == "__main__":
    main()
