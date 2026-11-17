#!/usr/bin/env python3
"""End-to-end checks of `trip upgrade` and the daemon's version record, each in
its own temporary HOME.

`trip upgrade` finds the newest release, runs that tag's installer, and checks
what it installed. Here a fake release source answers instead of GitHub: a
`gh` stand-in, a file:// copy of the releases API, and a file:// installer
that installs a stub `trip` printing the version it is told to. Nothing here
reaches the network.

The daemon writes ~/.trip/daemon.json with its version and pid, and removes it
when it exits. `trip ls`, `attach`, and `enter` print one line when the daemon
is older than the client, and a request an older daemon cannot decode fails
with that line. A `-dev` build is never older than a release, so these checks
build a release-marked trip of their own, with `TRIP_RELEASE=1`, into
target/upgrade-e2e.

Usage: tests/upgrade_e2e.py
"""
import fcntl
import json
import os
import pty
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RELEASE_TARGET = os.path.join(REPO, "target", "upgrade-e2e")
RELEASE = os.path.join(RELEASE_TARGET, "debug", "trip")
INSTALL = os.path.join(REPO, "install.sh")
LINE_END = "when its last session ends or you run trip shutdown --yes"

CARGO = shutil.which("cargo")
if not CARGO:
    sys.exit("upgrade_e2e: cargo is not on PATH")
# A temporary HOME would hide the toolchain and crate cache that cargo and
# rustup find under the real one.
TOOL_HOMES = {}
for name, default in (("CARGO_HOME", "~/.cargo"), ("RUSTUP_HOME", "~/.rustup")):
    path = os.environ.get(name) or os.path.expanduser(default)
    if os.path.isdir(path):
        TOOL_HOMES[name] = path

# Short and resolved: each daemon's socket lives under its HOME, and a unix
# socket path has to fit in SUN_LEN (~104 bytes).
ROOT = os.path.realpath(tempfile.mkdtemp(prefix="trip-upgrade-", dir="/tmp"))
failures = []
homes = []
daemons = []


def check(ok, what, detail=""):
    print(("ok   " if ok else "FAIL ") + what)
    if not ok:
        failures.append(what)
        if detail:
            print("     " + detail.replace("\n", "\n     "))


def outputs(result):
    return f"exit {result.returncode}\nstdout: {result.stdout}\nstderr: {result.stderr}"


def build_release():
    """A trip that reports a bare version, as the release installer builds it."""
    subprocess.run(
        [CARGO, "build", "--locked", "--target-dir", RELEASE_TARGET],
        cwd=REPO, env={**os.environ, "TRIP_RELEASE": "1"}, check=True,
    )
    said = subprocess.run([RELEASE, "--version"], capture_output=True, text=True, check=True).stdout.split()
    if said[-1].endswith("-dev"):
        sys.exit(f"upgrade_e2e: {RELEASE} reports {said[-1]}, not a release")
    return said[-1]


def write(path, text, mode=None):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(text)
    if mode:
        os.chmod(path, mode)


def read(path):
    try:
        with open(path) as f:
            return f.read()
    except FileNotFoundError:
        return None


def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def is_test_daemon(pid):
    """Whether `pid` is still a daemon these checks started, not a process
    that has since taken its number."""
    said = subprocess.run(["ps", "-p", str(pid), "-o", "command="], capture_output=True, text=True).stdout
    return said.strip() == f"{RELEASE} daemon"


class Home:
    """A fresh HOME, and a fake release source that stands in for GitHub."""

    count = 0

    def __init__(self):
        Home.count += 1
        self.dir = os.path.join(ROOT, f"h{Home.count}")
        self.source = os.path.join(ROOT, f"s{Home.count}")
        os.makedirs(self.dir)
        os.makedirs(self.source)
        self.bin = os.path.join(self.dir, ".local", "bin", "trip")
        self.record = os.path.join(self.dir, ".trip", "daemon.json")
        self.gh_calls = os.path.join(self.source, "gh-calls")
        self.installer_ran = os.path.join(self.source, "installer-ran")
        self.gh = os.path.join(self.source, "gh")
        # Answers only when a case writes the tag it should print.
        write(self.gh, f"""#!/bin/sh
echo "$*" >> {self.gh_calls}
if [ -f {self.source}/gh-tag ]; then cat {self.source}/gh-tag; exit 0; fi
echo "gh: not signed in" >&2
exit 1
""", 0o755)
        self.env = {
            "HOME": self.dir,
            "TMPDIR": os.path.join(self.source, "tmp"),
            "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
            "SHELL": "/bin/sh",
            "TERM": "xterm-256color",
            "LANG": "C",
            "TRIP_GH": self.gh,
            "TRIP_GITHUB_API": "file://" + os.path.join(self.source, "api"),
            "TRIP_RAW_URL": "file://" + os.path.join(self.source, "raw"),
        }
        os.makedirs(self.env["TMPDIR"])
        homes.append(self)

    def publish(self, tag, installs=None, repo="ninjudd/trip"):
        """Make `tag` the newest release of `repo`, with an installer that
        installs a trip reporting `installs`, the tag's version by default."""
        installs = installs or tag.lstrip("v")
        write(os.path.join(self.source, "api", "repos", repo, "releases", "latest"),
              json.dumps({"tag_name": tag, "name": tag.lstrip("v")}))
        write(os.path.join(self.source, "raw", repo, tag, "install.sh"), f"""#!/usr/bin/env bash
set -eu
printf '%s %s\\n' "$TRIP_REPO" "$TRIP_REF" > {self.installer_ran}
mkdir -p "$HOME/.local/bin"
rm -f "$HOME/.local/bin/trip"
printf '#!/bin/sh\\necho "trip {installs}"\\n' > "$HOME/.local/bin/trip"
chmod +x "$HOME/.local/bin/trip"
echo "Done."
""")

    def gh_answers(self, tag):
        write(os.path.join(self.source, "gh-tag"), tag + "\n")

    def release_json(self, repo):
        write(os.path.join(self.dir, ".local", "share", "trip", "release.json"),
              json.dumps({"repo": repo, "ref": "v0"}) + "\n")

    def run(self, *args, trip=RELEASE, timeout=30):
        return subprocess.run([trip, *args], capture_output=True, text=True,
                              env=self.env, cwd=self.dir, timeout=timeout)

    def start_daemon(self, *names):
        """Start this HOME's daemon with a session per name, and note its pid
        so the cleanup can stop it."""
        for name in names:
            result = self.run("create", name, "--", "/bin/sleep", "300")
            if result.returncode != 0:
                raise RuntimeError(outputs(result))
        record = json.loads(read(self.record))
        daemons.append(record["pid"])
        return record

    def stop(self):
        self.run("shutdown", "--yes")


class Term:
    """A client on a real PTY, its output drained so it never blocks."""

    def __init__(self, home, *args):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        self.proc = subprocess.Popen([RELEASE, *args], stdin=slave, stdout=slave, stderr=slave,
                                     env=home.env, cwd=home.dir, preexec_fn=os.setsid)
        os.close(slave)
        self.output = b""

    def drain(self, seconds):
        deadline = time.monotonic() + seconds
        while select.select([self.master], [], [], max(0, deadline - time.monotonic()))[0]:
            try:
                chunk = os.read(self.master, 65536)
            except OSError:
                return
            if not chunk:
                return
            self.output += chunk

    def detach(self):
        """Press the detach key twice, as a person leaves a session, and
        return what the client printed by the time it exits."""
        self.drain(1)
        os.write(self.master, b"\x1c")
        self.drain(0.5)
        os.write(self.master, b"\x1c")
        deadline = time.monotonic() + 10
        while self.proc.poll() is None and time.monotonic() < deadline:
            self.drain(0.1)
        self.drain(0.2)
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()
        os.close(self.master)
        return self.output.decode(errors="replace")


def older(daemon, client, sessions):
    plural = "session" if sessions == 1 else "sessions"
    return f"the daemon still runs {daemon} with {sessions} {plural}; it moves to {client} {LINE_END}"


# ---- trip upgrade ----

def test_upgrade_installs_the_newest_release(version):
    home = Home()
    home.release_json("someone/trip-fork")
    home.publish("v9.0.0", repo="someone/trip-fork")
    result = home.run("upgrade")
    check(result.returncode == 0, "upgrade succeeds", outputs(result))
    check(read(home.installer_ran) == "someone/trip-fork v9.0.0\n",
          "the tag's own installer runs with TRIP_REPO and TRIP_REF set to the fork and the tag",
          str(read(home.installer_ran)))
    check("repos/someone/trip-fork/releases/latest" in (read(home.gh_calls) or ""),
          "gh is asked for the fork's newest release first", str(read(home.gh_calls)))
    check(f"upgraded trip {version} to 9.0.0" in result.stdout,
          "it says which version it upgraded from and to", outputs(result))
    check("+ curl -fsSL file://" in result.stderr and "TRIP_REF=v9.0.0 bash" in result.stderr,
          "it prints the installer command it runs", result.stderr)


def test_gh_answers_before_the_anonymous_api(version):
    home = Home()
    home.publish("v9.1.0")
    # The anonymous API names an older tag than gh does.
    home.publish("v9.0.0")
    home.gh_answers("v9.1.0")
    result = home.run("upgrade")
    check(result.returncode == 0 and read(home.installer_ran) == "ninjudd/trip v9.1.0\n",
          "the tag gh names wins over the anonymous API's", outputs(result) + f"\n{read(home.installer_ran)}")


def test_mismatched_version_exits_1(version):
    home = Home()
    home.publish("v9.0.0", installs="8.0.0")
    # The installer keeps the version it switched away from as `previous`.
    share = os.path.join(home.dir, ".local", "share", "trip")
    os.makedirs(os.path.join(share, "0.0.9"))
    os.symlink("0.0.9", os.path.join(share, "previous"))
    result = home.run("upgrade")
    check(result.returncode == 1, "an install that reports another version exits 1", outputs(result))
    check(result.stderr.endswith("\nError: the upgrade installed trip 8.0.0, but the newest release, v9.0.0, "
                                 "is 9.0.0; trip 8.0.0 is now current; run ln -sfn 0.0.9 "
                                 "~/.local/share/trip/current to switch back to 0.0.9\n")
          and "upgraded" not in result.stdout,
          "and names both versions, the build now current, and how to switch back to the previous one",
          outputs(result))

    home = Home()
    home.publish("v9.0.0", installs="8.0.0")
    result = home.run("upgrade")
    check(result.returncode == 1
          and result.stderr.endswith("\nError: the upgrade installed trip 8.0.0, but the newest release, "
                                     "v9.0.0, is 9.0.0; trip 8.0.0 is now current\n"),
          "with no previous version it gives no command to switch back", outputs(result))


def test_check_installs_nothing(version):
    home = Home()
    home.publish("v9.0.0")
    result = home.run("upgrade", "--check")
    check(result.returncode == 0 and f"trip {version} is installed" in result.stdout
          and "newest release is 9.0.0" in result.stdout,
          "--check prints the installed version and the newest release", outputs(result))
    check(read(home.installer_ran) is None and not os.path.exists(home.bin), "--check installs nothing")


def test_newest_already_installed(version):
    home = Home()
    home.publish("v" + version)
    result = home.run("upgrade")
    check(result.returncode == 0 and read(home.installer_ran) is None,
          "an install already at the newest release runs no installer", outputs(result))


def test_no_release_found(version):
    home = Home()
    result = home.run("upgrade")
    check(result.returncode == 1 and "could not find the newest release of ninjudd/trip" in result.stderr
          and "not signed in" in result.stderr,
          "with no release to find it fails and says why each lookup failed", outputs(result))


def test_dev_install_is_refused(version):
    """`./install.sh --dev` over a release install, then `trip upgrade`."""
    home = Home()
    home.publish("v9.0.0")
    share = os.path.join(home.dir, ".local", "share", "trip")
    write(os.path.join(share, "0.0.9", "trip"), "#!/bin/sh\necho 'trip 0.0.9'\n", 0o755)
    os.symlink("0.0.9", os.path.join(share, "current"))
    os.makedirs(os.path.dirname(home.bin))
    os.symlink(os.path.join(share, "current", "trip"), home.bin)
    linked = subprocess.run(
        ["./install.sh", "--dev"], capture_output=True, text=True, cwd=REPO, timeout=600,
        env={**home.env, **TOOL_HOMES, "PATH": os.path.dirname(CARGO) + ":" + home.env["PATH"]},
    )
    target = os.readlink(home.bin) if os.path.islink(home.bin) else None
    check(linked.returncode == 0 and target is not None and target.endswith("/debug/trip"),
          "./install.sh --dev links ~/.local/bin/trip to the checkout's debug build", outputs(linked))
    result = home.run("upgrade", trip=home.bin)
    check(result.returncode != 0 and "checkout install" in result.stderr and "./install.sh" in result.stderr,
          "trip upgrade on a -dev install refuses and points to ./install.sh", outputs(result))
    check(os.readlink(home.bin) == target
          and os.readlink(os.path.join(share, "current")) == "0.0.9"
          and read(home.installer_ran) is None and read(home.gh_calls) is None,
          "and leaves the link and current alone without looking anything up")


def test_upgrade_leaves_an_older_daemon_running(version):
    home = Home()
    record = home.start_daemon("work")
    home.publish("v9.0.0")
    result = home.run("upgrade")
    check(result.returncode == 0, "upgrade beside a running daemon succeeds", outputs(result))
    check(older(version, "9.0.0", 1) in result.stderr,
          "and says the daemon still runs the old version, with its session count", result.stderr)
    check(alive(record["pid"]) and "work" in home.run("ls").stdout,
          "the daemon and its session are still running")
    home.stop()


# ---- the daemon's version record ----

def installer_reads(home):
    """What install.sh's own daemon_version function reads from the record."""
    script = f'DAEMON_RECORD="{home.record}"\n' + subprocess.run(
        ["sed", "-n", "/^daemon_version()/,/^}/p", INSTALL], capture_output=True, text=True, check=True,
    ).stdout + "daemon_version\n"
    return subprocess.run(["bash", "-c", script], capture_output=True, text=True).stdout.strip()


def test_record_written_and_removed(version):
    home = Home()
    record = home.start_daemon("work")
    check(record["version"] == version and alive(record["pid"]),
          "the daemon records its version and its own pid", str(record))
    check(installer_reads(home) == version, "install.sh reads the version from the record", installer_reads(home))
    listed = home.run("ls")
    check(listed.returncode == 0 and listed.stderr == "", "a current daemon adds no line to trip ls", outputs(listed))
    home.stop()
    check(read(home.record) is None, "daemon.json is gone once the daemon stops", str(read(home.record)))

    record = home.start_daemon("short")
    home.run("kill", "short")
    deadline = time.monotonic() + 5
    while alive(record["pid"]) and time.monotonic() < deadline:
        time.sleep(0.05)
    check(read(home.record) is None, "and once it exits with its last session", str(read(home.record)))


def test_older_daemon_line(version):
    home = Home()
    record = home.start_daemon("one", "two")
    write(home.record, json.dumps({"version": "0.0.9", "pid": record["pid"]}))
    listed = home.run("ls")
    check(listed.returncode == 0 and "one" in listed.stdout and "two" in listed.stdout,
          "trip ls still lists the older daemon's sessions", outputs(listed))
    check(listed.stderr == older("0.0.9", version, 2) + "\n",
          "and adds one line naming both versions and the session count", outputs(listed))

    attached = Term(home, "attach", "one").detach()
    check(older("0.0.9", version, 2) in attached, "trip attach prints it once it detaches", attached[-400:])
    entered = Term(home, "enter", "two").detach()
    check(older("0.0.9", version, 2) in entered, "and so does trip enter", entered[-400:])

    write(home.record, json.dumps({"version": version, "pid": record["pid"]}))
    check(home.run("ls").stderr == "", "a record at the client's version adds nothing")
    write(home.record, json.dumps({"version": "99.0.0", "pid": record["pid"]}))
    check(home.run("ls").stderr == "", "nor does a newer daemon")
    write(home.record, json.dumps({"version": "0.0.9-dev", "pid": record["pid"]}))
    check(home.run("ls").stderr == "", "nor a -dev daemon")
    home.stop()


def test_missing_record_is_from_before_releases(version):
    home = Home()
    home.start_daemon("work")
    os.remove(home.record)
    listed = home.run("ls")
    check(listed.stderr == f"the daemon still runs a trip from before releases with 1 session; "
                           f"it moves to {version} {LINE_END}\n",
          "a daemon with no record is named as from before releases", outputs(listed))
    home.stop()
    check(home.run("ls").stderr == "", "and with no daemon at all there is no line")


def test_dead_pid_record_is_ignored(version):
    home = Home()
    home.start_daemon("work")
    exited = subprocess.Popen(["true"])
    exited.wait()
    # A record at the client's own version would vouch for the daemon if its
    # pid were not checked.
    write(home.record, json.dumps({"version": version, "pid": exited.pid}))
    listed = home.run("ls")
    check("from before releases with 1 session" in listed.stderr,
          "a record whose pid is gone vouches for nothing", outputs(listed))
    home.stop()


def test_screen_and_log_against_a_current_daemon(version):
    """A current daemon answers every `trip screen` before it closes, but
    sends nothing for a log with no events, so only `trip screen` can read a
    close with no reply as a failure."""
    home = Home()
    home.start_daemon("work")
    screen = home.run("screen", "work")
    check(screen.returncode == 0 and screen.stderr == "", "trip screen of a session succeeds", outputs(screen))
    missing = home.run("screen", "nothing")
    check(missing.returncode == 1 and "session 'nothing' not found" in missing.stderr,
          "trip screen of a missing session names it", outputs(missing))
    empty = home.run("log", "nothing")
    check(empty.returncode == 0 and empty.stdout == "" and empty.stderr == "",
          "trip log with no events prints nothing and succeeds", outputs(empty))
    home.stop()


class OlderDaemon:
    """A daemon that lists sessions but closes the connection on any other
    request, as an older daemon does on a request it cannot decode."""

    def __init__(self, home, version, sessions):
        os.makedirs(os.path.dirname(home.record), exist_ok=True)
        self.path = os.path.join(home.dir, ".trip", "daemon.sock")
        write(home.record, json.dumps({"version": version, "pid": os.getpid()}))
        self.listing = json.dumps({"type": "SessionList", "sessions": [{
            "name": f"s{i}", "command": "sh", "pid": 1, "created_at": 0, "state": "Running",
            "attached": False, "cwd": None, "fg_command": None, "git_branch": None, "title": None,
        } for i in range(sessions)]}).encode()
        self.server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.server.bind(self.path)
        self.server.listen()
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self):
        while True:
            try:
                conn, _ = self.server.accept()
            except OSError:
                return
            with conn:
                head = conn.recv(5)
                if len(head) < 5:
                    continue
                length = struct.unpack(">I", head[:4])[0]
                body = b""
                while len(body) < length - 1:
                    body += conn.recv(length - 1 - len(body))
                if json.loads(body).get("type") == "ListSessions":
                    conn.sendall(struct.pack(">I", len(self.listing) + 1) + b"\x01" + self.listing)

    def close(self):
        self.server.close()
        os.remove(self.path)


def test_older_daemon_failing_a_request(version):
    home = Home()
    fake = OlderDaemon(home, "0.0.9", 3)
    try:
        result = home.run("kill", "anything")
        check(result.returncode != 0
              and f"the daemon could not answer this request: {older('0.0.9', version, 3)}" in result.stderr
              and "unexpected frame" not in result.stderr,
              "a request an older daemon cannot decode fails with the older-daemon line", outputs(result))
        result = home.run("screen", "anything")
        check(result.returncode != 0
              and f"the daemon could not answer this request: {older('0.0.9', version, 3)}" in result.stderr,
              "and so does trip screen, though it closes with no reply at all", outputs(result))
        write(home.record, json.dumps({"version": version, "pid": os.getpid()}))
        result = home.run("kill", "anything")
        check("unexpected frame" in result.stderr and "older" not in result.stderr
              and "could not answer" not in result.stderr,
              "a daemon at the client's version keeps the original error", outputs(result))
    finally:
        fake.close()


def main():
    version = build_release()
    try:
        for test in (
            test_upgrade_installs_the_newest_release,
            test_gh_answers_before_the_anonymous_api,
            test_mismatched_version_exits_1,
            test_check_installs_nothing,
            test_newest_already_installed,
            test_no_release_found,
            test_dev_install_is_refused,
            test_upgrade_leaves_an_older_daemon_running,
            test_record_written_and_removed,
            test_older_daemon_line,
            test_missing_record_is_from_before_releases,
            test_dead_pid_record_is_ignored,
            test_screen_and_log_against_a_current_daemon,
            test_older_daemon_failing_a_request,
        ):
            print(f"-- {test.__name__}")
            test(version)
    finally:
        for home in homes:
            if os.path.exists(os.path.join(home.dir, ".trip", "daemon.sock")):
                home.stop()
        for pid in daemons:
            if is_test_daemon(pid):
                os.kill(pid, signal.SIGKILL)
        shutil.rmtree(ROOT, ignore_errors=True)
    if failures:
        print(f"\n{len(failures)} check(s) failed")
        sys.exit(1)
    print("\nall upgrade checks passed")


if __name__ == "__main__":
    main()
