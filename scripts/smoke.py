#!/usr/bin/env python3
"""Real PTY smoke checks, including an isolated compositor event stream.

No real workspaces, compositor settings or running monitor are touched.
"""
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time


def drain(fd, seconds=0.2):
    end = time.monotonic() + seconds
    data = b""
    while time.monotonic() < end:
        if select.select([fd], [], [], min(0.05, max(0, end - time.monotonic())))[0]:
            data += os.read(fd, 1 << 20)
    return data


def read_calls(pid):
    return int(dict(line.split(": ") for line in Path(f"/proc/{pid}/io").read_text().splitlines())["syscr"])


def main():
    binary = str(Path(sys.argv[1]).resolve())
    with tempfile.TemporaryDirectory(prefix="power-monitor-smoke-", dir=os.environ.get("JCODE_SCRATCH_DIR")) as tmp:
        path = Path(tmp)
        fifo = path / "events"
        os.mkfifo(fifo)
        helper = path / "niri"
        helper.write_text('''#!/usr/bin/env python3
import json,os
print(json.dumps({"WorkspacesChanged":{"workspaces":[{"id":1,"output":"test","is_active":True}]}}),flush=True)
print(json.dumps({"WindowsChanged":{"windows":[{"id":1,"pid":int(os.environ["TEST_TERMINAL_PID"]),"app_id":"power-monitor","workspace_id":1,"layout":{"tile_pos_in_workspace_view":[0,0]}}]}}),flush=True)
with open(os.environ["TEST_EVENTS"]) as events:
    for line in events:
        print(line,end="",flush=True)
''')
        helper.chmod(0o755)
        env = dict(os.environ, PATH=tmp + os.pathsep + os.environ["PATH"],
                   NIRI_SOCKET="test-only", TEST_TERMINAL_PID=str(os.getpid()), TEST_EVENTS=str(fifo))
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
        proc = subprocess.Popen([binary], stdin=slave, stdout=slave, stderr=slave, env=env)
        try:
            with open(fifo, "w", buffering=1) as events:
                assert b"power" in drain(master, 1.5)
                # Active workspace, but tile scrolled off-screen.
                events.write(json.dumps({"WindowLayoutsChanged":{"changes":[[1,{"tile_pos_in_workspace_view":None}]]}}) + "\n")
                drain(master, 0.3)
                calls = read_calls(proc.pid)
                output = drain(master, 2.3)
                assert output == b"", f"hidden window wrote {len(output)} bytes"
                assert read_calls(proc.pid) == calls, "sampling continued while off-screen"
                events.write(json.dumps({"WindowLayoutsChanged":{"changes":[[1,{"tile_pos_in_workspace_view":[0,0]}]]}}) + "\n")
                assert drain(master, 1.3), "did not resume on visibility event"
                for columns, rows in [(64,12), (40,16), (24,16), (2,2), (120,30)]:
                    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))
                    os.kill(proc.pid, signal.SIGWINCH)
                    drain(master, 0.1)
                    assert proc.poll() is None, f"crashed resizing to {columns}x{rows}"
                os.kill(proc.pid, signal.SIGTERM)
                assert proc.wait(timeout=5) == 0
                assert not (termios.tcgetattr(slave)[3] & termios.ICANON) == 0, "raw terminal not restored"
                print("PASS: hidden tile stops reads/output, visible resumes, five resizes, SIGTERM restores terminal")
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()
            os.close(master)
            os.close(slave)


if __name__ == "__main__":
    main()
