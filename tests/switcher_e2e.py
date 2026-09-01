#!/usr/bin/env python3
"""End-to-end checks for the session switcher, in an isolated HOME.

Drives a real PTY, because every interesting acceptance criterion is a
keystroke into an attached client.
"""
import fcntl
import os
import pty
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TRIP = os.path.join(REPO, "target", "debug", "trip")

# Realpath, and short: the daemon's socket lives under HOME and a unix socket
# path has to fit in SUN_LEN (~104 bytes). Realpath also matters for its own
# sake -- `git rev-parse` reports a resolved path, and the session name is
# derived by stripping HOME off it, so a symlinked HOME would never match.
HOME = os.path.realpath(tempfile.mkdtemp(prefix="trip-e2e-", dir="/tmp"))
WS = f"{HOME}/ws/proj"
# What `derive_session_name` makes of the workspace directories below.
PROJ = "ws/proj"
OTHER = "ws/other"

DETACH = b"\x1c"

failures = []
notes = []


def check(name, ok, detail=""):
    print(("  PASS  " if ok else "  FAIL  ") + name + (f"\n        {detail}" if detail and not ok else ""))
    if not ok:
        failures.append(name)


def env():
    e = dict(os.environ)
    e["HOME"] = HOME
    e["SHELL"] = "/bin/sh"
    e["TERM"] = "xterm-256color"
    e.pop("TRIP_SESSION", None)
    e.pop("TRIP_WORKSPACE", None)
    e.pop("TRIP_DETACH_KEY", None)
    e.pop("TRIP_TITLE", None)
    return e


def trip(*args, cwd=WS, timeout=15):
    return subprocess.run(
        [TRIP, *args], env=env(), cwd=cwd, capture_output=True, text=True, timeout=timeout
    )


class Term:
    """An attached client on a real PTY."""

    def __init__(self, *args, cwd=WS, rows=24, cols=100):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.proc = subprocess.Popen(
            [TRIP, *args], stdin=slave, stdout=slave, stderr=slave,
            env=env(), cwd=cwd, preexec_fn=os.setsid,
        )
        os.close(slave)
        self.buf = ""

    def read(self, seconds=1.0):
        import select
        end = time.time() + seconds
        while time.time() < end:
            r, _, _ = select.select([self.master], [], [], 0.1)
            if r:
                try:
                    chunk = os.read(self.master, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                self.buf += chunk.decode("utf-8", "replace")
        return self.buf

    def send(self, data):
        os.write(self.master, data)
        time.sleep(0.45)

    def clear(self):
        self.buf = ""

    def close(self):
        try:
            self.proc.kill()
        except Exception:
            pass
        try:
            os.close(self.master)
        except Exception:
            pass


def plain(s):
    """Strip escape sequences so assertions read the text, not the paint."""
    s = re.sub(r"\x1b\][^\x07\x1b]*(\x07|\x1b\\)", "", s)
    s = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", s)
    s = re.sub(r"\x1b[=>]", "", s)
    return s


def setup():
    if not os.path.exists(TRIP):
        sys.exit(f"build it first: cargo build  (no {TRIP})")
    # cargo test does not rebuild the plain binary; a suite driving a stale
    # build reports on code that is not there. Refuse rather than mislead.
    newest = max(
        os.path.getmtime(os.path.join(root, f))
        for root, _, files in os.walk(os.path.join(REPO, "src"))
        for f in files
        if f.endswith(".rs")
    )
    if os.path.getmtime(TRIP) < newest:
        sys.exit("stale build: src/ is newer than the binary — run cargo build first")
    for name in ("proj", "other", "only", "fresh"):
        os.makedirs(f"{HOME}/ws/{name}")
        subprocess.run(["git", "init", "-q"], cwd=f"{HOME}/ws/{name}", capture_output=True)


def teardown():
    subprocess.run([TRIP, "shutdown", "--yes"], env=env(), capture_output=True)
    shutil.rmtree(HOME, ignore_errors=True)


def main():
    setup()
    try:
        # ---- listing ----
        trip("create", PROJ)
        trip("create", f"{PROJ}.1")
        trip("create", OTHER, cwd=f"{HOME}/ws/other")
        time.sleep(0.5)

        out = trip("ls").stdout
        check("trip ls spans workspaces", PROJ in out and OTHER in out, out)
        out = trip("ls", "--pwd").stdout
        check("trip ls --pwd narrows", PROJ in out and OTHER not in out, out)
        r = trip("ls", "-a")
        check("trip ls -a is an error", r.returncode == 2 and "unexpected argument" in r.stderr, r.stderr)
        r = trip("ls", "--attached", "--pwd")
        check("--attached composes with --pwd", r.returncode == 0, r.stderr)

        # ---- the key opens the chooser ----
        t = Term("attach", PROJ)
        t.read(1.5)
        t.clear()
        t.send(DETACH)
        screen = plain(t.read(1.5))
        check("detach key opens the chooser", "sessions:" in screen and "esc back" in screen, screen[-400:])
        check("hint names the configured key", "^\\ detach" in screen, screen[-400:])
        check("chooser lists other workspaces", OTHER in screen, screen[-400:])
        check("current session is marked", "(current)" in screen, screen[-400:])
        check("row 0 offers a new session", "(new session)" in screen, screen[-400:])

        # The attached client is fully raw (OPOST off): a renderer emitting
        # bare LF stair-steps the list across the screen. Check the raw bytes
        # of the paint, from the chooser's clear-screen onward.
        raw = t.buf
        paint = raw[raw.rindex("\x1b[2J\x1b[H"):]
        bare = [i for i, _ in enumerate(paint)
                if paint[i] == "\n" and (i == 0 or paint[i - 1] != "\r")]
        check("chooser lines all return the carriage", not bare,
              f"bare LF at {bare[:3]} in {paint[:200]!r}")

        # ---- esc goes back ----
        t.clear()
        t.send(b"\x1b")
        time.sleep(0.4)
        screen = t.read(1.5)
        check("esc leaves the chooser", "sessions:" not in plain(screen), plain(screen)[-300:])
        alive = t.proc.poll() is None
        check("esc does not exit the client", alive)

        # ---- switch to another session ----
        t.clear()
        t.send(DETACH)
        before = plain(t.read(1.2))
        t.send(b"\x1b[B")   # down
        t.send(b"\r")
        after = t.read(2.0)
        time.sleep(0.6)
        ls = trip("ls").stdout
        attached = [l for l in ls.splitlines() if l.strip().startswith("+")]
        check("switch moved the client to another session",
              any(f"{PROJ}.1" in l for l in attached), f"before={before[-300:]}\nls=\n{ls}")

        # ---- cancelling must not eat the return ----
        # Three round trips through the chooser, each one a self-switch. Without
        # the condition on the return-stack push, each leaves proj.1 on its own
        # stack and `trip return` no-ops instead of going back to proj.
        for _ in range(3):
            t.send(DETACH)
            t.read(0.8)
            t.send(b"\x1b")
            t.read(0.8)

        # ---- trip return still goes to the session we switched from ----
        rr = subprocess.run([TRIP, "return"], env={**env(), "TRIP_SESSION": f"{PROJ}.1"},
                            cwd=WS, capture_output=True, text=True)
        time.sleep(0.8)
        ls = trip("ls").stdout
        attached = [l for l in ls.splitlines() if l.strip().startswith("+")]
        check("trip return lands on the session we switched from, not a cancel",
              any(re.search(rf"\s{re.escape(PROJ)}\s", l) for l in attached),
              f"return={rr.stdout}{rr.stderr}\nls=\n{ls}")
        check("one return was enough after three cancels",
              not any(f"{PROJ}.1" in l for l in attached), f"ls=\n{ls}")

        # ---- up+enter creates the next numbered session ----
        t.clear()
        t.send(DETACH)
        t.read(1.2)
        t.send(b"\x1b[A")   # up, onto row 1
        t.send(b"\r")
        t.read(2.0)
        time.sleep(0.8)
        ls = trip("ls").stdout
        check("up+enter created the next numbered session", f"{PROJ}.2" in ls, ls)

        # ---- the key still works under an enhanced keyboard protocol ----
        # Claude Code switches the terminal into kitty CSI-u / modifyOtherKeys
        # at startup, after which the detach key arrives as an escape sequence
        # rather than a byte. Synthesize what the terminal would send: the
        # default key is ^\ (0x1c), whose CSI-u spelling is 92;5u.
        t.send(b"\x1b[92;5u")
        screen = plain(t.read(1.5))
        check("a CSI-u encoded detach key opens the chooser",
              "esc back" in screen, screen[-300:])
        t.clear()
        t.send(b"\x1b[27u")   # Esc, as the kitty protocol spells it
        t.read(1.5)
        check("a CSI-u encoded Esc cancels", t.proc.poll() is None)
        # And the modifyOtherKeys spelling opens it too.
        t.send(b"\x1b[27;5;92~")
        screen2 = plain(t.read(1.5))
        check("a modifyOtherKeys encoded detach key opens the chooser",
              "esc back" in screen2[len(screen):] or "esc back" in screen2,
              screen2[-300:])
        t.send(b"\x1b")
        t.read(1.0)

        # ---- digit 0 creates from anywhere ----
        before0 = set(re.findall(rf"{re.escape(PROJ)}\.\d+", trip("ls").stdout))
        t.send(DETACH)
        t.read(1.2)
        t.send(b"0")
        t.read(2.0)
        time.sleep(0.8)
        after0 = set(re.findall(rf"{re.escape(PROJ)}\.\d+", trip("ls").stdout))
        check("digit 0 creates the next session", len(after0 - before0) == 1,
              sorted(after0 - before0))

        # ---- the key twice detaches ----
        t.clear()
        t.send(DETACH)
        t.read(1.0)
        t.send(DETACH)
        out = plain(t.read(2.0))
        t.proc.wait(timeout=5)
        check("key twice detaches", "[detached:" in out, out[-300:])
        check("client exited", t.proc.returncode == 0, str(t.proc.returncode))
        t.close()

        # ---- only the terminal that pressed it moves ----
        a = Term("attach", PROJ)
        b = Term("attach", PROJ)
        a.read(1.2)
        b.read(1.2)
        b.clear()
        a.send(DETACH)
        a.read(1.0)
        a.send(b"\x1b[B")
        a.send(b"\r")
        a.read(1.5)
        time.sleep(0.8)
        bs = plain(b.read(0.8))
        check("the other terminal never saw a chooser", "sessions:" not in bs, bs[-300:])
        check("the other terminal is still running", b.proc.poll() is None)
        a.close()
        b.close()

        # ---- the PTY refits to whoever is left ----
        # A small terminal and a large one hold one session; the PTY fits the
        # smaller. When the small one switches away, the large one should get
        # its room back.
        trip("create", "fit")
        time.sleep(0.4)
        small = Term("attach", "fit", rows=24, cols=80)
        large = Term("attach", "fit", rows=40, cols=100)
        small.read(1.2)
        large.read(1.2)
        trip("send", "fit", "stty size")
        time.sleep(0.8)
        fitted = trip("screen", "fit").stdout
        check("the PTY fits the smaller of two terminals", "24 80" in fitted, fitted[-200:])

        small.send(DETACH)
        small.read(1.0)
        small.send(b"\x1b[B")
        small.send(b"\r")
        small.read(1.5)
        large.read(1.0)
        time.sleep(1.0)
        trip("send", "fit", "stty size")
        time.sleep(0.8)
        large.read(0.5)
        refitted = trip("screen", "fit").stdout
        check("the PTY refits once the smaller terminal switches away",
              "40 100" in refitted, refitted[-200:])
        small.close()
        large.close()

        # ---- a long list scrolls instead of walking off the top ----
        for i in range(3, 20):
            trip("create", f"{PROJ}.{i}")
        time.sleep(1.0)
        t = Term("attach", PROJ, rows=12, cols=100)
        t.read(1.5)
        t.clear()
        t.send(DETACH)
        screen = plain(t.read(1.5))
        painted = [l for l in screen.splitlines() if re.match(r"^[ >]\s*\d\)", l)]
        check("long list is windowed, not printed whole", 0 < len(painted) <= 10, f"{len(painted)} rows")
        check("truncation marker shown", "⋯" in screen, screen[-500:])
        t.close()

        # ---- a created session inherits the cwd of the one it came from ----
        trip("send", PROJ, "cd /usr/lib")
        time.sleep(0.8)
        t2 = Term("attach", PROJ)
        t2.read(1.5)
        t2.send(DETACH)
        t2.read(1.2)
        t2.send(b"\x1b[A")
        t2.send(b"\r")
        t2.read(2.0)
        time.sleep(1.0)
        ls = trip("ls").stdout
        made = [l for l in ls.splitlines() if re.search(rf"{re.escape(PROJ)}\.\d+", l) and "/usr/lib" in l]
        check("a session made from the chooser starts where its parent was",
              bool(made), ls)
        t2.close()

        # ---- two terminals racing on the same displayed number ----
        before = set(re.findall(rf"{re.escape(PROJ)}\.\d+", trip("ls").stdout))
        racers = [Term("attach", PROJ) for _ in range(2)]
        for r_ in racers:
            r_.read(1.2)

        def step(data, settle):
            """Write to both, then drain both.

            The draining is not optional: a chooser over twenty-odd sessions
            paints several KB, and a PTY nobody reads fills up and blocks the
            client mid-write, so it never gets to the keystroke.
            """
            for r_ in racers:
                os.write(r_.master, data)
            end = time.time() + settle
            while time.time() < end:
                for r_ in racers:
                    r_.read(0.15)

        step(DETACH, 1.2)
        step(b"\x1b[A", 0.6)
        step(b"\r", 2.5)

        after = set(re.findall(rf"{re.escape(PROJ)}\.\d+", trip("ls").stdout))
        check("two terminals racing both get a session, on different numbers",
              len(after - before) == 2, f"new={sorted(after - before)}")
        for r_ in racers:
            r_.close()

        # ---- an empty workspace is one keystroke ----
        t3 = Term("enter", cwd=f"{HOME}/ws/fresh")
        screen = plain(t3.read(2.5))
        check("a fresh workspace preselects its create row",
              "(new session)" in screen, screen[-400:])
        t3.send(b"\r")
        t3.read(2.0)
        time.sleep(0.8)
        check("enter on that row creates the canonical session",
              "fresh" in trip("ls").stdout, trip("ls").stdout)
        t3.close()

        # ---- esc on an exited session must not destroy it ----
        # Cancel used to run the full detach/re-attach round trip, whose
        # bookkeeping can GC an exited session in the gap where this client is
        # not counted -- Esc would destroy the session it was declining to
        # leave, or take the daemon with it. Constructing the state is fussy:
        # a session that exits while attached persists as exited, but only
        # until the daemon next spawns a child (`trip ls` runs git for the
        # branch column), whose SIGCHLD sweeps exited clientless sessions. So
        # no ls between the kick and the re-attach.
        trip("create", "dead", "--", "/bin/sleep", "1")
        time.sleep(0.3)
        t = Term("attach", "dead")
        t.read(1.2)
        t.proc.wait(timeout=10)  # the session exits and this client is kicked

        t = Term("attach", "dead")
        t.read(1.2)
        check("an exited session that died while attached can be re-attached",
              t.proc.poll() is None)
        t.send(DETACH)
        t.read(1.0)
        t.send(b"\x1b")
        t.read(1.5)
        check("esc on an exited session leaves the client running", t.proc.poll() is None)
        # This client holds it, so the ls-triggered sweep keeps its hands off.
        check("and the session still exists", "dead" in trip("ls").stdout, trip("ls").stdout)
        t.close()

        # ---- esc still works while the session floods output ----
        # The escape timeout is a deadline; a timer restarted per dropped
        # frame never fires under continuous output.
        trip("create", "noisy")
        time.sleep(0.4)
        trip("send", "noisy", "while :; do echo spam; sleep 0.01; done")
        time.sleep(0.5)
        t = Term("attach", "noisy")
        t.read(1.2)
        t.send(DETACH)
        t.read(1.0)
        t.clear()
        t.send(b"\x1b")
        screen = plain(t.read(2.0))
        check("esc lands while the session floods output",
              "spam" in screen and t.proc.poll() is None, screen[-200:])
        t.send(b"\x03")  # stop the flood
        t.read(0.5)
        t.close()

        # ---- a mouse click while the chooser is up picks nothing ----
        t = Term("attach", PROJ)
        t.read(1.2)
        t.send(DETACH)
        t.read(1.0)
        before_click = set(re.findall(rf"{re.escape(PROJ)}\.\d+", trip("ls").stdout))
        t.send(b"\x1b[<0;12;5M")   # SGR mouse press, as vim would have enabled
        t.read(1.0)
        after_click = set(re.findall(rf"{re.escape(PROJ)}\.\d+", trip("ls").stdout))
        check("a mouse click in the chooser creates nothing",
              before_click == after_click, sorted(after_click - before_click))
        t.send(b"\x1b")
        t.read(1.0)
        t.close()

        # ---- a live app keeps its input modes across a cancel ----
        trip("create", "modes")
        time.sleep(0.4)
        # The session's own program turns bracketed paste on, the way an
        # editor would.
        trip("send", "modes", "printf '\\033[?2004h'")
        time.sleep(0.6)
        t = Term("attach", "modes")
        t.read(1.5)
        t.clear()
        t.send(DETACH)
        t.read(1.2)
        t.send(b"\x1b")
        raw = t.read(2.0)
        check("cancel restores bracketed paste to the live app",
              "\x1b[?2004h" in raw, repr(raw[-300:]))
        t.close()

        # ---- the title follows a switch ----
        t = Term("attach", PROJ)
        t.read(1.5)
        t.clear()
        t.send(DETACH)
        t.read(1.2)
        t.send(b"\x1b[B")
        t.send(b"\r")
        raw = t.read(2.0)
        titles = re.findall(r"\x1b\]1;([^\x07]*)\x07", raw)
        check("switching retitles the terminal", any("proj" in x for x in titles), repr(titles))
        t.close()

        # ---- --pwd skips the chooser when there is nothing to choose ----
        t = Term("enter", "--pwd", cwd=f"{HOME}/ws/only")
        screen = plain(t.read(2.5))
        check("enter --pwd attaches directly with one canonical session",
              "sessions:" not in screen, screen[-300:])
        t.close()

        # ---- non-tty enter takes the canonical session ----
        r = subprocess.run([TRIP, "enter"], env=env(), cwd=WS, capture_output=True,
                           text=True, stdin=subprocess.DEVNULL, timeout=10)
        check("enter with stdin redirected prints no chooser",
              "sessions:" not in r.stdout + r.stderr, (r.stdout + r.stderr)[-300:])
    finally:
        teardown()

    print()
    if failures:
        print(f"FAILED: {len(failures)}")
        for f in failures:
            print(f"  - {f}")
        sys.exit(1)
    print("all checks passed")


if __name__ == "__main__":
    main()
