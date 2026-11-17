#!/usr/bin/env python3
"""Set trip's version, check a release commit, or tag a release.

    .github/scripts/release.py bump patch|minor|major  raise one part of the version, write it, and print it
    .github/scripts/release.py set X.Y.Z               write an exact version instead
    .github/scripts/release.py check-commit [REV]      refuse a commit that changes anything but the version
    .github/scripts/release.py tag [--commit REV] [--since REV]
                                                       tag vX.Y.Z, publish its GitHub release, and move v0

The Release workflow runs these. Started by hand with patch, minor, or major,
it runs `bump` and `check-commit` on a release/vX.Y.Z branch and opens the
release pull request. When that pull request merges, it runs `tag` on the
merge commit, with --since naming main's previous head, so a push that
leaves the version alone or lowers it, as a revert does, releases nothing.
`tag` also refuses a new version below the newest vX.Y.Z tag, and never moves
the major tag back to an older version. `tag` pushes the immutable vX.Y.Z
tag, creates the GitHub release for it with generated notes (--no-release
skips that step), and then force-moves the major tag that the installer
follows. The major tag moves last because `trip upgrade` reads the newest
GitHub release, so once the major tag names a release, `trip upgrade`
already finds that release.

A version is released once its tag and the major tag name the same commit and
its GitHub release exists. `tag` does only the steps the remote lacks, in that
order, so running it again finishes a release that stopped partway.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tomllib
from pathlib import Path
from typing import Callable, NamedTuple

ROOT = Path(__file__).resolve().parents[2]
VERSION_FILES = ("Cargo.lock", "Cargo.toml")
SEMVER = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")
# The version line of Cargo.toml's [package] table, before any other table starts.
PACKAGE_VERSION = re.compile(r'(?ms)(^\[package\]\n(?:(?!^\[).)*?^version\s*=\s*")[^"]*(")')
# trip's own entry in Cargo.lock, which cargo writes as name, then version.
LOCK_VERSION = re.compile(r'(?m)(^\[\[package\]\]\nname = "trip"\nversion = ")[^"]*(")')
PATTERNS = {"Cargo.toml": PACKAGE_VERSION, "Cargo.lock": LOCK_VERSION}
Reader = Callable[[str], str]


class ReleaseError(Exception):
    pass


def parse(version: str) -> tuple[int, int, int]:
    match = SEMVER.match(version)
    if not match:
        raise ReleaseError(f"{version!r} is not X.Y.Z")
    major, minor, patch = (int(part) for part in match.groups())
    return major, minor, patch


def reader(root: Path) -> Reader:
    return lambda name: (root / name).read_text()


def versions(read: Reader) -> dict[str, str]:
    found = {}
    try:
        found["Cargo.toml"] = tomllib.loads(read("Cargo.toml"))["package"]["version"]
    except KeyError as exc:
        raise ReleaseError("Cargo.toml has no [package] version") from exc
    entries = [entry for entry in tomllib.loads(read("Cargo.lock")).get("package", []) if entry.get("name") == "trip"]
    if len(entries) != 1:
        raise ReleaseError(f"Cargo.lock has {len(entries) or 'no'} trip package entries, not one")
    found["Cargo.lock"] = entries[0]["version"]
    return found


def current(read: Reader) -> str:
    found = versions(read)
    if len(set(found.values())) != 1:
        raise ReleaseError("the version files disagree: " + ", ".join(f"{k}={v}" for k, v in found.items()))
    return found["Cargo.toml"]


def with_version(name: str, text: str, version: str) -> str:
    text, count = PATTERNS[name].subn(rf"\g<1>{version}\g<2>", text, count=1)
    if count != 1:
        raise ReleaseError(f"{name} has no trip version line")
    return text


def set_version(root: Path, version: str) -> None:
    read = reader(root)
    old = current(read)
    if parse(version) <= parse(old):
        raise ReleaseError(f"{version} is not above the current version {old}")
    # Both files are rewritten in memory first, so a failure writes neither.
    texts = {name: with_version(name, read(name), version) for name in VERSION_FILES}
    for name, text in texts.items():
        (root / name).write_text(text)


def bump(root: Path, part: str) -> str:
    """Raise the patch, minor, or major number, write it, and return the new version."""
    major, minor, patch = parse(current(reader(root)))
    version = {
        "patch": f"{major}.{minor}.{patch + 1}",
        "minor": f"{major}.{minor + 1}.0",
        "major": f"{major + 1}.0.0",
    }[part]
    set_version(root, version)
    return version


def git(root: Path, *args: str) -> str:
    try:
        return subprocess.run(["git", *args], cwd=root, check=True, capture_output=True, text=True).stdout.strip()
    except subprocess.CalledProcessError as exc:
        raise ReleaseError(f"git {' '.join(args)} failed: {exc.stderr.strip()}") from exc


def at(root: Path, commit: str) -> Reader:
    return lambda name: git(root, "show", f"{commit}:{name}") + "\n"


def version_at(root: Path, commit: str) -> str:
    return current(at(root, commit))


def check_commit(root: Path, rev: str = "HEAD") -> str:
    """Refuse a commit unless it raises the version and changes nothing else."""
    commit = git(root, "rev-parse", "--verify", f"{rev}^{{commit}}")
    changed = sorted(git(root, "diff", "--name-only", f"{commit}^", commit).split())
    if changed != sorted(VERSION_FILES):
        raise ReleaseError(f"the release commit changes {' '.join(changed) or 'nothing'}, "
                           f"not only {' '.join(sorted(VERSION_FILES))}")
    old, new = version_at(root, f"{commit}^"), version_at(root, commit)
    if parse(new) <= parse(old):
        raise ReleaseError(f"the release commit moves the version from {old} to {new}, not up")
    before, after = at(root, f"{commit}^"), at(root, commit)
    for name in VERSION_FILES:
        if after(name) != with_version(name, before(name), new):
            raise ReleaseError(f"the release commit changes {name} beyond its version")
    return new


def repo_slug(url: str) -> str:
    url = url.strip()
    for prefix in ("git@github.com:", "ssh://git@github.com/", "https://github.com/"):
        if url.startswith(prefix):
            url = url[len(prefix):]
            return url[:-4] if url.endswith(".git") else url
    return ""


def gh(root: Path, remote: str, *args: str) -> subprocess.CompletedProcess[str]:
    slug = repo_slug(git(root, "remote", "get-url", remote))
    if not slug:
        raise ReleaseError(f"{remote} is not a GitHub repository")
    try:
        return subprocess.run(["gh", *args, "--repo", slug], cwd=root, capture_output=True, text=True)
    except FileNotFoundError as exc:
        raise ReleaseError("the gh CLI is not installed") from exc


def published(root: Path, remote: str, release: str) -> bool:
    result = gh(root, remote, "release", "view", release, "--json", "tagName")
    if result.returncode == 0:
        return True
    if "release not found" in result.stderr:
        return False
    raise ReleaseError(f"gh release view failed: {result.stderr.strip()}")


def create_release(root: Path, remote: str, release: str) -> None:
    result = gh(root, remote, "release", "create", release, "--verify-tag", "--generate-notes")
    if result.returncode:
        raise ReleaseError(f"gh release create failed: {result.stderr.strip()}")


class Merged(NamedTuple):
    """The version a commit names, the commit that releases it, and which of its tags the remote has."""

    version: str
    name: str
    major: str
    commit: str
    tagged: bool
    major_moved: bool
    major_version: str | None


def remote_tags(root: Path, remote: str, *names: str) -> dict[str, str]:
    """The commit each named tag peels to on the remote, for the names the remote has."""
    patterns = [pattern for name in names for pattern in (f"refs/tags/{name}", f"refs/tags/{name}^{{}}")]
    commits: dict[str, str] = {}
    for line in git(root, "ls-remote", "--tags", remote, *patterns).splitlines():
        sha, ref = line.split("\t")
        name = ref.removeprefix("refs/tags/")
        if name.endswith("^{}"):
            commits[name.removesuffix("^{}")] = sha
        else:
            commits.setdefault(name, sha)
    return commits


def tagged_versions(root: Path, remote: str) -> list[str]:
    """The versions the remote's vX.Y.Z tags name."""
    found = []
    for line in git(root, "ls-remote", "--tags", "--refs", remote, "refs/tags/v*").splitlines():
        version = line.split("\t")[1].removeprefix("refs/tags/v")
        if SEMVER.match(version):
            found.append(version)
    return found


def is_ancestor(root: Path, commit: str, head: str) -> bool:
    return subprocess.run(["git", "merge-base", "--is-ancestor", commit, head], cwd=root,
                          capture_output=True).returncode == 0


def check_tagged(root: Path, head: str, version: str, tagged: str) -> None:
    """Refuse a version tag unless the history has named that version from the tagged commit to head."""
    if not is_ancestor(root, tagged, head):
        raise ReleaseError(f"v{version} already names {tagged[:9]}, which is not in {head[:9]}'s history; "
                           "set a new version first")
    changes = git(root, "log", "--format=%H", f"{tagged}..{head}", "--", *VERSION_FILES).split()
    for change in (tagged, *changes):
        named = version_at(root, change)
        if named != version:
            raise ReleaseError(f"v{version} already names {tagged[:9]}, but the history names {named} "
                               f"at {change[:9]}, so that tag belongs to another release; set a new version first")


def merged(root: Path, remote: str, branch: str, commit: str | None) -> Merged:
    """Read the release commit's version and which of its tags the remote already has.

    The release commit is `commit`, or the branch's head on the remote when it
    is None. Once the version tag exists, the tagged commit releases it, so a
    release that stopped partway finishes there.
    """
    # A failed push can leave this checkout with a tag the remote refused.
    # --force replaces it with the remote's rather than failing the fetch.
    git(root, "fetch", "--quiet", "--force", "--tags", remote, branch)
    head = git(root, "rev-parse", f"refs/remotes/{remote}/{branch}")
    if commit is not None:
        given = git(root, "rev-parse", "--verify", f"{commit}^{{commit}}")
        if not is_ancestor(root, given, head):
            raise ReleaseError(f"{commit} is not on {remote}/{branch}")
        head = given
    version = version_at(root, head)
    name, major = f"v{version}", f"v{parse(version)[0]}"
    tags = remote_tags(root, remote, name, major)
    release_commit = tags.get(name, head)
    if name in tags:
        check_tagged(root, head, version, release_commit)
    else:
        # A version below a released one would move the major tag back to older code.
        newest = max(tagged_versions(root, remote), key=parse, default=None)
        if newest is not None and parse(newest) > parse(version):
            raise ReleaseError(f"v{newest} is already released, so {version} cannot be; "
                               f"set a version above {newest}")
    major_version = version_at(root, tags[major]) if major in tags else None
    return Merged(version, name, major, release_commit, tagged=name in tags,
                  major_moved=tags.get(major) == release_commit, major_version=major_version)


def tag(root: Path, remote: str = "origin", branch: str = "main", commit: str | None = None,
        since: str | None = None, github_release: bool = True) -> str:
    if since is not None and commit is not None:
        # Only a push that raises the version is a release; a revert lowers it.
        version, before = version_at(root, commit), version_at(root, since)
        if before == version:
            return f"{since[:9]}..{commit[:9]} leaves the version at {version}, so there is nothing to release"
        if parse(before) > parse(version):
            return (f"{since[:9]}..{commit[:9]} lowers the version from {before} to {version}, "
                    "so there is nothing to release")
    state = merged(root, remote, branch, commit)
    did: list[str] = []
    found: list[str] = []
    try:
        if state.tagged:
            found.append(f"the {state.name} tag")
        else:
            # --force replaces only a local tag that a failed push left behind.
            # The push has no --force, so it never replaces a version tag.
            git(root, "tag", "--force", "-a", state.name, state.commit, "-m", f"trip {state.version}")
            git(root, "push", "--quiet", remote, f"refs/tags/{state.name}")
            did.append(f"tagged {state.name}")
        if github_release:
            if published(root, remote, state.name):
                found.append("the GitHub release")
            else:
                create_release(root, remote, state.name)
                did.append("published the GitHub release")
        if state.major_moved:
            found.append(state.major)
        elif state.major_version is not None and parse(state.major_version) > parse(state.version):
            # Finishing an older release after a newer one must not take the installer back.
            did.append(f"left {state.major} on the newer {state.major_version}")
        else:
            git(root, "tag", "--force", "-a", state.major, state.commit, "-m", f"trip {state.version}")
            git(root, "push", "--quiet", "--force", remote, f"refs/tags/{state.major}")
            did.append(f"moved {state.major}")
    except ReleaseError as exc:
        before = f" Before that it {', '.join(did)}." if did else ""
        raise ReleaseError(f"{exc}.{before} Run tag again, or re-run the Release workflow, "
                           f"to finish {state.name}") from exc
    summary = f"{state.name} at {state.commit[:9]}: {', '.join(did) or 'nothing to do'}"
    return summary + (f" (already done: {', '.join(found)})" if found else "")


def main(argv: list[str] | None = None, root: Path = ROOT) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    s = sub.add_parser("set", help="write a new version to Cargo.toml and Cargo.lock")
    s.add_argument("version")
    b = sub.add_parser("bump", help="raise the patch, minor, or major number and print the new version")
    b.add_argument("part", choices=("patch", "minor", "major"))
    c = sub.add_parser("check-commit", help="refuse a commit that changes anything but the version")
    c.add_argument("rev", nargs="?", default="HEAD")
    t = sub.add_parser("tag", help="do the release steps the remote lacks: the version tag, the GitHub release, the major tag")
    t.add_argument("--remote", default="origin")
    t.add_argument("--branch", default="main")
    t.add_argument("--commit", help="the release commit, which must be on the branch (default: the branch's head)")
    t.add_argument("--since", help="release nothing unless --commit names a higher version than this commit")
    t.add_argument("--no-release", action="store_true", help="push the tags without creating the GitHub release")
    args = parser.parse_args(argv)
    try:
        if args.command == "set":
            set_version(root, args.version)
            print(f"set {args.version} in Cargo.toml and Cargo.lock")
        elif args.command == "bump":
            print(bump(root, args.part))
        elif args.command == "check-commit":
            print(f"the release commit changes only the version, to {check_commit(root, args.rev)}")
        else:
            if args.since is not None and args.commit is None:
                parser.error("--since needs --commit")
            print(tag(root, args.remote, args.branch, args.commit, args.since, github_release=not args.no_release))
    except ReleaseError as exc:
        print(f"release: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
