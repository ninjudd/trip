#!/usr/bin/env python3
"""End-to-end check that entering a session whose shell exited gives you a
live shell rather than its corpse, in an isolated HOME.

An exited session stays in the daemon until the next SIGCHLD sweeps it, so
that `trip attach` can show what its command left behind. `trip enter` used to
attach to that corpse too: you saw the old screen and scrollback, keystrokes
and `trip send` went nowhere, and `enter` never returned. That is what a
reconnect looks like after Ctrl-D in a remote `trip enter <name>`.

Usage: tests/exited_session_e2e.py [path to trip binary]
"""
import fcntl
import os
import pty
import select
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
HOME = os.path.realpath(tempfile.mkdtemp(prefix="trip-exited-", dir="/tmp"))
ENV = dict(os.environ, HOME=HOME, SHELL="/bin/sh", TERM="xterm-256color")
ENV.pop("TRIP_SESSION", None)
ENV.pop("TRIP_WORKSPACE", None)
# The default title template, so a switch titles the terminal with the name.
ENV.pop("TRIP_TITLE", None)


def trip(*args):
    return subprocess.run([TRIP, *args], env=ENV, capture_output=True, text=True, timeout=20)


class Term:
    """A client on a real PTY, its output drained so it never blocks."""

    def __init__(self, *args):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        self.proc = subprocess.Popen(
            [TRIP, *args], stdin=slave, stdout=slave, stderr=slave,
            env=ENV, cwd=HOME, preexec_fn=os.setsid,
        )
        os.close(slave)
        self.output = b""

    def drain(self, seconds=0):
        """Read what the client has written, for up to this long."""
        deadline = time.monotonic() + seconds
        while select.select([self.master], [], [], max(0, deadline - time.monotonic()))[0]:
            try:
                chunk = os.read(self.master, 65536)
            except OSError:
                return
            if not chunk:
                return
            self.output += chunk

    def wait_for(self, text, timeout=10):
        """Drain until the client has drawn this text. The client sets raw mode
        before it draws anything, which flushes input typed earlier."""
        deadline = time.monotonic() + timeout
        while text not in self.output and time.monotonic() < deadline:
            self.drain(0.1)
        return text in self.output

    def send(self, data):
        os.write(self.master, data)

    def wait(self, timeout):
        """Drain until the client exits; return its exit code, or None."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.drain()
            if self.proc.poll() is not None:
                return self.proc.returncode
            time.sleep(0.05)
        return None

    def close(self):
        self.proc.kill()
        os.close(self.master)


def wait_for_screen(name, text, timeout=10):
    """Poll `trip screen` until it shows this text; return the last result."""
    deadline = time.monotonic() + timeout
    while True:
        screen = trip("screen", name)
        if (screen.returncode == 0 and text in screen.stdout) or time.monotonic() > deadline:
            return screen
        time.sleep(0.1)


failures = []


def check(name, ok, detail=""):
    print(("  PASS  " if ok else "  FAIL  ") + name + (f"\n        {detail}" if detail and not ok else ""))
    if not ok:
        failures.append(name)


clients = []
try:
    # A second session keeps the daemon running once the shell exits, as
    # every other session does on a real machine.
    trip("create", "keep", "--", "sleep", "60")

    first = Term("enter", "shell")
    clients.append(first)
    first.wait_for(b"$")
    first.send(b"echo first-$((6*7))\r")
    screen = wait_for_screen("shell", "first-42")
    check("the first shell runs", "first-42" in screen.stdout, (screen.stdout + screen.stderr).strip())

    # Ctrl-D ends the shell, and the daemon releases its client.
    first.send(b"\x04")
    check("logging out releases the client", first.wait(10) == 0, str(first.proc.returncode))

    # Reconnect at once, with no `trip ls` in between: its SIGCHLD would
    # sweep the corpse and hide the bug.
    second = Term("enter", "shell")
    clients.append(second)
    second.wait_for(b"$")
    second.send(b"echo second-$((6*7))\r")
    screen = wait_for_screen("shell", "second-42")
    check("entering it again gives a live shell", "second-42" in screen.stdout,
          (screen.stdout + screen.stderr).strip())
    second.send(b"\x04")
    check("and logging out of that one releases its client too", second.wait(10) == 0,
          str(second.proc.returncode))

    # From inside another session, `enter` switches this terminal rather than
    # attaching a new one, and that path has to skip the corpse too.
    work = Term("enter", "work")
    clients.append(work)
    work.wait_for(b"$")
    work.send(TRIP.encode() + b" enter shell\r")
    # The client retitles the terminal once the switch lands.
    check("the switch lands", work.wait_for(b"\x1b]1;shell\x07"))
    work.send(b"echo inside-$((6*7))\r")
    screen = wait_for_screen("shell", "inside-42")
    check("entering it from inside a session gives a live shell", "inside-42" in screen.stdout,
          (screen.stdout + screen.stderr).strip())

    # `trip attach` still shows a corpse on purpose. Input sent to one has
    # nowhere to go, so say so instead of reporting success.
    trip("create", "dead", "--", "/bin/sh", "-c", "echo last words; sleep 1")
    kicked = Term("attach", "dead")
    clients.append(kicked)
    kicked.wait(10)
    viewer = Term("attach", "dead")
    clients.append(viewer)
    viewer.wait_for(b"last words")
    check("attach still shows an exited session", viewer.proc.poll() is None
          and "last words" in trip("screen", "dead").stdout)
    sent = trip("send", "dead", "hello")
    check("trip send to an exited session fails", sent.returncode != 0 and "exited" in sent.stderr,
          f"exit {sent.returncode}: {(sent.stdout + sent.stderr).strip()}")
finally:
    trip("shutdown", "--yes")
    for client in clients:
        client.close()
    shutil.rmtree(HOME, ignore_errors=True)

print("\nall checks passed" if not failures else f"\n{len(failures)} check(s) failed")
sys.exit(1 if failures else 0)
