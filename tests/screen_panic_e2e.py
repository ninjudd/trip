#!/usr/bin/env python3
"""End-to-end check that a session whose screen vt100 cannot draw leaves the
daemon answering for every session, in an isolated HOME.

vt100 0.15 panicked when a program saved the cursor, the window shrank, and
the program restored the cursor below the new last row and printed. The panic
poisoned that session's screen lock, and `trip ls` failed with "unexpected
frame" until the daemon restarted.

Usage: tests/screen_panic_e2e.py [path to trip binary]
"""
import fcntl
import os
import pty
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TRIP = os.path.abspath(sys.argv[1]) if len(sys.argv) > 1 else os.path.join(REPO, "target", "debug", "trip")

# Short and resolved: the daemon's socket lives under HOME, and a unix socket
# path has to fit in SUN_LEN (~104 bytes).
HOME = os.path.realpath(tempfile.mkdtemp(prefix="trip-panic-", dir="/tmp"))
ENV = dict(os.environ, HOME=HOME, SHELL="/bin/sh", TERM="xterm-256color")
ENV.pop("TRIP_SESSION", None)
ENV.pop("TRIP_WORKSPACE", None)

# Saves the cursor on row 45 while a 49-row client holds the PTY, then restores
# it and prints once a 25-row client has shrunk the PTY. Each escape is its own
# printf because "\0337" would read as one octal byte.
PROGRAM = 'sleep 2; printf "\\033[45;10H\\033"; printf 7; sleep 3; printf "\\033"; printf 8a; sleep 60'


def trip(*args):
    return subprocess.run([TRIP, *args], env=ENV, capture_output=True, text=True, timeout=20)


def attach(name, rows, cols):
    """Attach a client from a terminal of this size; the PTY fits the smallest."""
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    proc = subprocess.Popen(
        [TRIP, "attach", name], stdin=slave, stdout=slave, stderr=slave,
        env=ENV, preexec_fn=os.setsid,
    )
    os.close(slave)
    return proc, master


failures = []


def check(name, ok, detail=""):
    print(("  PASS  " if ok else "  FAIL  ") + name + (f"\n        {detail}" if detail and not ok else ""))
    if not ok:
        failures.append(name)


clients = []
try:
    # A second session keeps the daemon running once the first one breaks,
    # which is what turned one bad screen into every command failing.
    trip("create", "healthy", "--", "sleep", "60")
    trip("create", "redraw", "--", "sh", "-c", PROGRAM)
    clients.append(attach("redraw", 49, 158))
    time.sleep(3.5)
    clients.append(attach("redraw", 25, 81))
    time.sleep(3)

    ls = trip("ls")
    check("trip ls lists every session", ls.returncode == 0 and "healthy" in ls.stdout and "redraw" in ls.stdout,
          (ls.stdout + ls.stderr).strip())
    screen = trip("screen", "redraw")
    check("the session's screen still renders", screen.returncode == 0 and "a" in screen.stdout,
          (screen.stdout + screen.stderr).strip())
    log = open(f"{HOME}/.trip/daemon.log").read()
    check("the daemon logged no panic", "panicked" not in log, log.strip())
finally:
    trip("shutdown", "--yes")
    for proc, _ in clients:
        proc.kill()
    shutil.rmtree(HOME, ignore_errors=True)

print("\nall checks passed" if not failures else f"\n{len(failures)} check(s) failed")
sys.exit(1 if failures else 0)
