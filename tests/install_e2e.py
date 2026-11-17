#!/usr/bin/env python3
"""End-to-end checks of install.sh, each in its own temporary HOME.

The installer builds each version into ~/.local/share/trip/<version>/, switches
`current` to it only once the build succeeds, links ~/.local/bin/trip through
`current`, keeps the version before, and never removes the one a running
daemon uses. Most cases install a tiny stand-in crate named trip, which builds
in a second; one installs trip's own source, to show that a release build
reports a bare version.

Usage: tests/install_e2e.py [path to install.sh]
"""
import os
import shutil
import signal
import subprocess
import sys
import tarfile
import tempfile
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
INSTALL = os.path.abspath(sys.argv[1]) if len(sys.argv) > 1 else os.path.join(REPO, "install.sh")
MARKER = "# trip shell hook"

CARGO = shutil.which("cargo")
if not CARGO:
    sys.exit("install_e2e: cargo is not on PATH")
# Only cargo's directory and the system's, so ~/.local/bin is on PATH only
# when a case puts it there.
BASE_PATH = os.pathsep.join([os.path.dirname(CARGO), "/usr/bin", "/bin", "/usr/sbin", "/sbin"])
# The temporary HOME would hide the toolchain and crate cache that cargo and
# rustup find under the real one.
TOOL_HOMES = {}
for name, default in (("CARGO_HOME", "~/.cargo"), ("RUSTUP_HOME", "~/.rustup")):
    path = os.environ.get(name) or os.path.expanduser(default)
    if os.path.isdir(path):
        TOOL_HOMES[name] = path

ROOT = os.path.realpath(tempfile.mkdtemp(prefix="trip-install-"))
failures = []
children = []


def check(ok, what, detail=""):
    print(("ok   " if ok else "FAIL ") + what)
    if not ok:
        failures.append(what)
        if detail:
            print("     " + detail.replace("\n", "\n     "))


class Home:
    """A fresh HOME and TMPDIR, and the environment that confines the
    installer to them."""

    count = 0

    def __init__(self):
        Home.count += 1
        self.dir = os.path.join(ROOT, f"home{Home.count}")
        self.tmp = os.path.join(ROOT, f"tmp{Home.count}")
        os.makedirs(self.dir)
        os.makedirs(self.tmp)
        self.share = os.path.join(self.dir, ".local", "share", "trip")
        self.bin = os.path.join(self.dir, ".local", "bin", "trip")
        self.legacy = os.path.join(self.dir, "usr-local-bin-trip")
        self.env = {
            "HOME": self.dir,
            "TMPDIR": self.tmp,
            "PATH": BASE_PATH,
            "TRIP_LEGACY_BIN": self.legacy,
            "LANG": "C",
            **TOOL_HOMES,
        }

    def release(self, archive, *args, **env):
        """Run the installer as `curl ... | bash` does, reading it from stdin."""
        with open(INSTALL, "rb") as script:
            return subprocess.run(
                ["bash", "-s", "--", *args], stdin=script, capture_output=True, text=True,
                env={**self.env, "TRIP_ARCHIVE": "file://" + archive, "TRIP_REF": "vtest", **env},
                cwd=self.dir, timeout=900,
            )

    def checkout(self, checkout, *args, **env):
        """Run ./install.sh in a checkout."""
        return subprocess.run(
            ["./install.sh", *args], capture_output=True, text=True,
            env={**self.env, **env}, cwd=checkout, timeout=900,
        )

    def versions(self):
        if not os.path.isdir(self.share):
            return []
        return sorted(
            name for name in os.listdir(self.share)
            if os.path.isdir(os.path.join(self.share, name))
            and not os.path.islink(os.path.join(self.share, name))
        )

    def link(self, name):
        path = os.path.join(self.share, name)
        return os.readlink(path) if os.path.islink(path) else None

    def version(self):
        """What ~/.local/bin/trip says it is."""
        result = subprocess.run([self.bin, "--version"], capture_output=True, text=True, timeout=20)
        return result.stdout.strip()

    def read(self, name):
        with open(os.path.join(self.dir, name)) as f:
            return f.read()


def stand_in(version, broken=False):
    """A checkout of a crate named trip that prints only its version, the way
    trip does: bare only when TRIP_RELEASE=1 was set at build time."""
    path = os.path.join(ROOT, f"src-{version}{'-broken' if broken else ''}")
    if os.path.isdir(path):
        return path
    os.makedirs(os.path.join(path, "src"))
    with open(os.path.join(path, "Cargo.toml"), "w") as f:
        f.write(f'[package]\nname = "trip"\nversion = "{version}"\nedition = "2021"\n')
    with open(os.path.join(path, "Cargo.lock"), "w") as f:
        f.write(f'version = 3\n\n[[package]]\nname = "trip"\nversion = "{version}"\n')
    with open(os.path.join(path, "src", "main.rs"), "w") as f:
        f.write(
            "fn main() {\n"
            '    let release = option_env!("TRIP_RELEASE") == Some("1");\n'
            '    if std::env::args().nth(1).as_deref() == Some("--version") {\n'
            '        let suffix = if release { "" } else { "-dev" };\n'
            '        println!("trip {}{}", env!("CARGO_PKG_VERSION"), suffix);\n'
            "    }\n"
            "}\n"
        )
        if broken:
            f.write("this does not compile\n")
    shutil.copy(INSTALL, os.path.join(path, "install.sh"))
    return path


def archive(source, names=None):
    """The source as GitHub serves an archive: one top-level directory."""
    name = os.path.basename(source)
    path = os.path.join(ROOT, name + ".tar.gz")
    with tarfile.open(path, "w:gz") as tar:
        for entry in names or sorted(os.listdir(source)):
            tar.add(os.path.join(source, entry), arcname=f"{name}/{entry}")
    return path


def release(version, broken=False):
    """A stand-in's source archive."""
    return archive(stand_in(version, broken))


def outputs(result):
    return result.stdout + result.stderr


def test_release_of_trip_itself():
    """The release mode builds trip's own source with TRIP_RELEASE=1, so the
    installed trip reports the bare version, through `current`."""
    home = Home()
    source = archive(REPO, ["Cargo.toml", "Cargo.lock", "src"])
    with open(os.path.join(REPO, "Cargo.toml")) as f:
        version = next(line.split('"')[1] for line in f if line.startswith("version = "))
    started = time.monotonic()
    result = home.release(source)
    elapsed = time.monotonic() - started
    print(f"     release install of trip {version} took {elapsed:.0f}s")
    check(result.returncode == 0, "release install of trip's source succeeds", outputs(result))
    check(home.link("current") == version, f"current names {version}", str(home.link("current")))
    check(os.readlink(home.bin) == os.path.join(home.share, "current", "trip"),
          "~/.local/bin/trip links through current", str(os.path.islink(home.bin) and os.readlink(home.bin)))
    check(home.version() == f"trip {version}", "the installed trip reports a bare version", home.version())
    with open(os.path.join(home.share, "release.json")) as f:
        record = f.read()
    check('"repo": "ninjudd/trip"' in record and '"ref": "vtest"' in record,
          "release.json records the repository and ref", record)


def test_failed_build_keeps_current():
    home = Home()
    first = home.release(release("0.2.0"))
    check(first.returncode == 0, "a stand-in release installs", outputs(first))
    check(home.version() == "trip 0.2.0", "the stand-in release reports a bare version", home.version())
    result = home.release(release("0.3.0", broken=True))
    check(result.returncode != 0, "a failed build exits nonzero", outputs(result))
    check("0.3.0" in result.stderr and "did not install" in result.stderr
          and "0.2.0 is still current" in result.stderr,
          "it says which version did not install and what is still current", result.stderr)
    check(home.link("current") == "0.2.0", "current still names 0.2.0", str(home.link("current")))
    check(home.versions() == ["0.2.0"] and not [n for n in os.listdir(home.share) if n.startswith(".")],
          "no partial version directory remains", str(os.listdir(home.share)))
    check(os.listdir(home.tmp) == [], "the temporary build directory is gone", str(os.listdir(home.tmp)))
    check(home.version() == "trip 0.2.0", "~/.local/bin/trip still runs 0.2.0", home.version())


def test_archive_without_cargo_toml_names_the_ref():
    """An archive of another repository, as a typo in TRIP_REPO downloads,
    extracts like trip's but holds no trip Cargo.toml at its top level."""
    home = Home()
    source = os.path.join(ROOT, "src-not-trip")
    os.makedirs(source)
    with open(os.path.join(source, "README.md"), "w") as f:
        f.write("not trip\n")
    result = home.release(archive(source))
    check(result.returncode != 0, "an archive without trip's Cargo.toml exits nonzero", outputs(result))
    check("trip vtest" in result.stderr and "did not install" in result.stderr
          and "is not trip's source" in result.stderr and "Nothing was installed" in result.stderr,
          "it names the ref that did not install and why", result.stderr)


def test_three_installs_keep_two():
    home = Home()
    # A binary copied there by hand, as trip was installed before releases.
    os.makedirs(os.path.dirname(home.bin))
    with open(home.bin, "w") as f:
        f.write("hand-copied trip\n")
    for version in ("0.2.0", "0.3.0", "0.4.0"):
        result = home.release(release(version))
        check(result.returncode == 0, f"release {version} installs", outputs(result))
    check(os.path.islink(home.bin), "a hand-copied ~/.local/bin/trip becomes the link")
    check(home.versions() == ["0.3.0", "0.4.0"], "three installs keep the last two", str(home.versions()))
    check(home.link("current") == "0.4.0" and home.link("previous") == "0.3.0",
          "current names 0.4.0 and previous names 0.3.0", f"{home.link('current')} {home.link('previous')}")
    again = home.release(release("0.4.0"))
    check(again.returncode == 0 and home.versions() == ["0.3.0", "0.4.0"] and home.link("previous") == "0.3.0",
          "reinstalling the current version keeps the one before it", f"{home.versions()} {outputs(again)}")


def test_current_switched_by_hand_to_an_absolute_path_is_kept():
    """A user who rolls back with `ln -sfn ~/.local/share/trip/0.2.0 current`
    leaves current naming an absolute path rather than a version."""
    home = Home()
    for version in ("0.2.0", "0.3.0"):
        home.release(release(version))
    current = os.path.join(home.share, "current")
    os.remove(current)
    os.symlink(os.path.join(home.share, "0.2.0"), current)
    result = home.release(release("0.4.0"))
    check(result.returncode == 0, "an install after a hand-made current link succeeds", outputs(result))
    check((home.versions(), home.link("current"), home.link("previous")) == (["0.2.0", "0.4.0"], "0.4.0", "0.2.0"),
          "the version the hand-made link named becomes previous and is kept",
          f"{home.versions()} {home.link('current')} {home.link('previous')}")


def daemon_record(home, version, pid):
    os.makedirs(os.path.join(home.dir, ".trip"), exist_ok=True)
    with open(os.path.join(home.dir, ".trip", "daemon.json"), "w") as f:
        f.write(f'{{"version": "{version}", "pid": {pid}}}\n')


def test_live_daemon_version_is_kept():
    home = Home()
    for version in ("0.2.0", "0.3.0"):
        home.release(release(version))
    daemon = subprocess.Popen(["sleep", "300"])
    children.append(daemon)
    daemon_record(home, "0.2.0", daemon.pid)
    result = home.release(release("0.4.0"))
    check(result.returncode == 0, "an install beside a live daemon succeeds", outputs(result))
    check(home.versions() == ["0.2.0", "0.3.0", "0.4.0"],
          "the version a live daemon runs is kept", str(home.versions()))
    check("Kept trip 0.2.0" in result.stdout, "it says why it kept that version", result.stdout)
    daemon.kill()
    daemon.wait()


def test_stale_daemon_record_is_ignored():
    home = Home()
    for version in ("0.2.0", "0.3.0"):
        home.release(release(version))
    exited = subprocess.Popen(["true"])
    exited.wait()
    daemon_record(home, "0.2.0", exited.pid)
    result = home.release(release("0.4.0"))
    check(result.returncode == 0, "an install beside a stale daemon record succeeds", outputs(result))
    check(home.versions() == ["0.3.0", "0.4.0"],
          "a record whose pid is not running keeps nothing", str(home.versions()))


def test_warnings():
    home = Home()
    quiet = home.release(release("0.2.0"),
                         PATH=os.pathsep.join([os.path.dirname(home.bin), BASE_PATH]))
    check("warning" not in outputs(quiet), "no warning with ~/.local/bin on PATH and no old binary", outputs(quiet))

    with open(home.legacy, "w") as f:
        f.write("old trip\n")
    result = home.release(release("0.2.0"))
    out = outputs(result)
    check(f"{os.path.dirname(home.bin)} is not on PATH" in out
          and 'export PATH="$HOME/.local/bin:$PATH"' in out,
          "it warns that ~/.local/bin is not on PATH and prints the export line", out)
    check(f"sudo rm {home.legacy}" in out, "it warns about the old binary and prints the sudo rm", out)
    check(os.path.exists(home.legacy), "the old binary is left in place")
    rc = home.read(".zshrc") + home.read(".bashrc")
    check("export PATH" not in rc, "no startup file's PATH changed", rc)


OLD_HOOK = """# trip shell hook
if [ -n "$TRIP_SESSION" ]; then
  _trip_preexec() { eval "$(trip env)"; }
  if [ -n "$ZSH_VERSION" ]; then
    preexec_functions+=(_trip_preexec)
  elif [ -n "$BASH_VERSION" ]; then
    trap '_trip_preexec' DEBUG
  fi
fi
"""


def test_shell_hook():
    home = Home()
    dotfiles = os.path.join(home.dir, "dotfiles")
    os.makedirs(dotfiles)
    with open(os.path.join(dotfiles, "bashrc"), "w") as f:
        f.write("alias ll='ls -l'\n")
    os.symlink(os.path.join(dotfiles, "bashrc"), os.path.join(home.dir, ".bashrc"))
    with open(os.path.join(home.dir, ".zshrc"), "w") as f:
        f.write("export EDITOR=vi\n\n" + OLD_HOOK + "setopt autocd\n")

    for _ in range(2):
        result = home.release(release("0.2.0"))
        check(result.returncode == 0, "an install with existing startup files succeeds", outputs(result))
    zshrc, bashrc = home.read(".zshrc"), home.read(".bashrc")
    for name, text in ((".zshrc", zshrc), (".bashrc", bashrc)):
        check(text.count(MARKER) == 1, f"{name} has one hook after two installs", text)
        check("command -v trip" in text, f"{name}'s hook runs only when trip resolves", text)
    check("export EDITOR=vi" in zshrc and "setopt autocd" in zshrc and "eval \"$(trip env)\"; }" not in zshrc,
          "the old unguarded hook is replaced and the lines around it kept", zshrc)
    check("alias ll" in bashrc and os.path.islink(os.path.join(home.dir, ".bashrc")),
          "a symlinked .bashrc stays a symlink and keeps its lines", bashrc)

    # A session started as ~/.local/bin/trip enter can run a shell whose PATH
    # lacks ~/.local/bin, where the old hook failed before every command.
    old = source_without_trip(home, OLD_HOOK)
    check("not found" in old.stderr, "the old hook fails there, which the guard prevents", old.stderr)
    guarded = source_without_trip(home, bashrc)
    check((guarded.stdout, guarded.stderr) == ("ran\n", ""),
          "the hook stays quiet in a shell where trip does not resolve", f"{guarded.stdout!r} {guarded.stderr!r}")


def source_without_trip(home, startup):
    """Source a startup file in a trip session's bash whose PATH lacks trip,
    then run one command."""
    path = os.path.join(home.dir, "startup-under-test")
    with open(path, "w") as f:
        f.write(startup)
    return subprocess.run(
        ["bash", "-c", f'TRIP_SESSION=demo; . "{path}"; echo ran'],
        capture_output=True, text=True, env={"PATH": "/usr/bin:/bin", "HOME": home.dir}, timeout=20,
    )


def test_checkout_installs_dev_version():
    home = Home()
    checkout = stand_in("0.5.0")
    result = home.checkout(checkout, TRIP_RELEASE="1")
    check(result.returncode == 0, "./install.sh in a checkout succeeds", outputs(result))
    check(home.link("current") == "0.5.0-dev", "a checkout installs as <version>-dev", str(home.link("current")))
    check(home.version() == "trip 0.5.0-dev", "even with TRIP_RELEASE=1 in the environment", home.version())
    check(not os.path.exists(os.path.join(home.share, "release.json")), "a checkout records no release")


def test_dev_links_debug_build():
    home = Home()
    home.release(release("0.2.0"))
    before = (home.versions(), home.link("current"), sorted(os.listdir(home.share)))
    checkout = stand_in("0.6.0")
    result = home.checkout(checkout, "--dev")
    check(result.returncode == 0, "./install.sh --dev succeeds", outputs(result))
    target = os.path.join(checkout, "target", "debug", "trip")
    check(os.readlink(home.bin) == target, "--dev links ~/.local/bin/trip to target/debug/trip",
          str(os.path.islink(home.bin) and os.readlink(home.bin)))
    check(home.version() == "trip 0.6.0-dev", "the linked build reports <version>-dev", home.version())
    after = (home.versions(), home.link("current"), sorted(os.listdir(home.share)))
    check(after == before, "--dev leaves ~/.local/share/trip as it was", f"{before} -> {after}")

    piped = home.release(release("0.2.0"), "--dev")
    check(piped.returncode != 0 and "checkout" in piped.stderr, "--dev without a checkout refuses", outputs(piped))


def test_refuses_without_cargo():
    home = Home()
    result = home.release(release("0.2.0"), PATH="/usr/bin:/bin")
    check(result.returncode != 0 and "cargo" in result.stderr and "rustup" in result.stderr,
          "without cargo it refuses and says how to install Rust", outputs(result))
    check(not os.path.exists(os.path.join(home.dir, ".local")), "and installs nothing")


def main():
    if shutil.which("cargo", path="/usr/bin:/bin"):
        sys.exit("install_e2e: cargo in /usr/bin or /bin defeats the no-cargo case")
    try:
        for test in (
            test_failed_build_keeps_current,
            test_archive_without_cargo_toml_names_the_ref,
            test_three_installs_keep_two,
            test_current_switched_by_hand_to_an_absolute_path_is_kept,
            test_live_daemon_version_is_kept,
            test_stale_daemon_record_is_ignored,
            test_warnings,
            test_shell_hook,
            test_checkout_installs_dev_version,
            test_dev_links_debug_build,
            test_refuses_without_cargo,
            test_release_of_trip_itself,
        ):
            print(f"-- {test.__name__}")
            test()
    finally:
        for child in children:
            if child.poll() is None:
                os.kill(child.pid, signal.SIGKILL)
                child.wait()
        shutil.rmtree(ROOT, ignore_errors=True)
    if failures:
        print(f"\n{len(failures)} check(s) failed")
        sys.exit(1)
    print("\nall install checks passed")


if __name__ == "__main__":
    main()
