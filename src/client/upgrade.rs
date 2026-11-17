//! `trip upgrade`: install the newest release with that release's own
//! installer, check what it installed, and say what became of the daemon,
//! which keeps running the version it started with.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

use super::{release_numbers, warn_if_daemon_older};
use crate::cli::VERSION;

const UPSTREAM: &str = "ninjudd/trip";
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(15);

pub async fn upgrade(check: bool) -> Result<()> {
    let checkout = VERSION.ends_with("-dev");
    if checkout && !check {
        bail!(
            "trip {} is a checkout install, which follows its checkout rather than releases; \
             run ./install.sh in the checkout to update it",
            VERSION
        );
    }

    let repo = release_repo()?;
    let tag = newest_tag(&repo).await?;
    let newest = tag.strip_prefix('v').unwrap_or(&tag);

    if check {
        if checkout {
            println!(
                "trip {} is a checkout install; the newest release is {}",
                VERSION, newest
            );
        } else if is_newer(newest, VERSION) {
            println!(
                "trip {} is installed; the newest release is {}; run trip upgrade to install it",
                VERSION, newest
            );
        } else {
            println!(
                "trip {} is installed, and the newest release is {}",
                VERSION, newest
            );
        }
        return Ok(());
    }

    if !is_newer(newest, VERSION) {
        println!(
            "trip {} is installed, and the newest release is {}",
            VERSION, newest
        );
        warn_if_daemon_older(VERSION).await;
        return Ok(());
    }

    run_installer(&repo, &tag).await?;

    let installed = installed_version().await?;
    if installed != newest {
        bail!(
            "the upgrade installed trip {}, but the newest release, {}, is {}",
            installed,
            tag,
            newest
        );
    }
    println!("upgraded trip {} to {}", VERSION, installed);
    warn_if_daemon_older(&installed).await;
    Ok(())
}

/// Whether `candidate` is a newer release than `installed`. A version that is
/// not `X.Y.Z` counts as newer, so an odd tag still installs and the check of
/// what it installed decides.
fn is_newer(candidate: &str, installed: &str) -> bool {
    match (release_numbers(candidate), release_numbers(installed)) {
        (Some(candidate), Some(installed)) => candidate > installed,
        _ => candidate != installed,
    }
}

fn share_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("trip")
}

/// The repository the release install came from, as `release.json` records
/// it, so a fork upgrades from the fork.
fn release_repo() -> Result<String> {
    let path = share_dir().join("release.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(UPSTREAM.into()),
        Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
    };
    repo_from_record(&text).with_context(|| format!("{} names no repository", path.display()))
}

fn repo_from_record(text: &str) -> Option<String> {
    let record: serde_json::Value = serde_json::from_str(text).ok()?;
    let repo = record.get("repo")?.as_str()?;
    plain_repo(repo).then(|| repo.to_string())
}

/// `owner/name`, as install.sh accepts it, so it fits in a URL as it is.
fn plain_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    plain_ref(owner) && plain_ref(name)
}

/// A tag or name that fits in a URL path as it is.
fn plain_ref(name: &str) -> bool {
    name.bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Run a command and keep its output, giving up after `LOOKUP_TIMEOUT`.
async fn output(command: &mut Command) -> Result<std::process::Output> {
    command.kill_on_drop(true);
    match tokio::time::timeout(LOOKUP_TIMEOUT, command.output()).await {
        Ok(output) => Ok(output?),
        Err(_) => bail!("no answer in {} seconds", LOOKUP_TIMEOUT.as_secs()),
    }
}

/// The tag of the repository's newest release: through `gh` when it is signed
/// in, which spares the anonymous rate limit, and otherwise through the same
/// API anonymously with `curl`. `TRIP_GH` and `TRIP_GITHUB_API` replace the
/// two, so a test can answer without GitHub.
async fn newest_tag(repo: &str) -> Result<String> {
    let endpoint = format!("repos/{}/releases/latest", repo);
    let mut reasons = Vec::new();

    let gh = std::env::var("TRIP_GH").unwrap_or_else(|_| "gh".into());
    match output(Command::new(&gh).args([
        "api",
        "--hostname",
        "github.com",
        &endpoint,
        "--jq",
        ".tag_name",
    ]))
    .await
    {
        Ok(out) if out.status.success() => {
            let tag = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if plain_ref(&tag) {
                return Ok(tag);
            }
            reasons.push(format!("gh: the newest release names no tag ({:?})", tag));
        }
        Ok(out) => {
            let said = String::from_utf8_lossy(&out.stderr);
            reasons.push(format!(
                "gh: {}",
                said.lines().last().unwrap_or("failed").trim()
            ));
        }
        Err(e) => reasons.push(format!("gh: {}", e)),
    }

    let api = std::env::var("TRIP_GITHUB_API").unwrap_or_else(|_| "https://api.github.com".into());
    let url = format!("{}/{}", api.trim_end_matches('/'), endpoint);
    match output(Command::new("curl").args([
        "-fsSL",
        "-H",
        "Accept: application/vnd.github+json",
        &url,
    ]))
    .await
    {
        Ok(out) if out.status.success() => match tag_from_release(&out.stdout) {
            Some(tag) => return Ok(tag),
            None => reasons.push(format!("{}: the newest release names no tag", url)),
        },
        Ok(out) => {
            let said = String::from_utf8_lossy(&out.stderr);
            reasons.push(format!(
                "{}: {}",
                url,
                said.lines().last().unwrap_or("failed").trim()
            ));
        }
        Err(e) => reasons.push(format!("curl: {}", e)),
    }

    bail!(
        "could not find the newest release of {}: {}",
        repo,
        reasons.join("; ")
    )
}

fn tag_from_release(body: &[u8]) -> Option<String> {
    let release: serde_json::Value = serde_json::from_slice(body).ok()?;
    let tag = release.get("tag_name")?.as_str()?;
    plain_ref(tag).then(|| tag.to_string())
}

/// Download the tag's own install.sh and run it with `TRIP_REF` set to the
/// tag, so the installer and the source it builds come from one tag that
/// never moves. `TRIP_RAW_URL` replaces where it comes from, for tests.
async fn run_installer(repo: &str, tag: &str) -> Result<()> {
    let raw = std::env::var("TRIP_RAW_URL")
        .unwrap_or_else(|_| "https://raw.githubusercontent.com".into());
    let url = format!("{}/{}/{}/install.sh", raw.trim_end_matches('/'), repo, tag);

    // A directory only this user can enter, made fresh, so no one else can
    // swap the installer between the download and the run. A directory left
    // at this name is removed first; one that reappears fails the create.
    let scratch = std::env::temp_dir().join(format!("trip-upgrade-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
        .create(&scratch)
        .with_context(|| format!("could not create {}", scratch.display()))?;
    let result = download_and_run(&url, &scratch.join("install.sh"), repo, tag).await;
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

async fn download_and_run(
    url: &str,
    script: &std::path::Path,
    repo: &str,
    tag: &str,
) -> Result<()> {
    let fetched = output(
        Command::new("curl")
            .arg("-fsSL")
            .arg(url)
            .arg("-o")
            .arg(script),
    )
    .await?;
    if !fetched.status.success() {
        bail!(
            "could not download the installer from {}: {}; trip {} is still installed",
            url,
            String::from_utf8_lossy(&fetched.stderr).trim(),
            VERSION
        );
    }

    eprintln!(
        "+ curl -fsSL {} | TRIP_REPO={} TRIP_REF={} bash",
        url, repo, tag
    );
    let status = Command::new("bash")
        .arg(script)
        .env("TRIP_REPO", repo)
        .env("TRIP_REF", tag)
        .status()
        .await
        .context("could not run bash")?;
    if !status.success() {
        bail!(
            "the installer for {} failed; trip {} is still installed",
            tag,
            VERSION
        );
    }
    Ok(())
}

/// What the installed `~/.local/bin/trip --version` says it is.
async fn installed_version() -> Result<String> {
    let home = std::env::var("HOME").expect("HOME not set");
    let bin = PathBuf::from(home).join(".local").join("bin").join("trip");
    let out = output(Command::new(&bin).arg("--version"))
        .await
        .with_context(|| format!("could not run {} --version", bin.display()))?;
    let said = String::from_utf8_lossy(&out.stdout);
    match said.split_whitespace().last() {
        Some(version) if out.status.success() => Ok(version.to_string()),
        _ => bail!("{} --version printed no version", bin.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_is_newer_only_by_its_numbers() {
        assert!(is_newer("0.3.0", "0.2.9"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.2.0", "0.3.0"));
        // An odd tag installs, and the version check decides.
        assert!(is_newer("0.3.0-rc1", "0.2.0"));
        assert!(!is_newer("0.3.0-rc1", "0.3.0-rc1"));
    }

    #[test]
    fn the_record_names_an_owner_and_repository() {
        assert_eq!(
            repo_from_record(r#"{"repo": "someone/trip", "ref": "v0"}"#).as_deref(),
            Some("someone/trip")
        );
        for record in [
            r#"{"ref": "v0"}"#,
            r#"{"repo": "trip"}"#,
            r#"{"repo": "a/b/c"}"#,
            r#"{"repo": "a/b c"}"#,
            r#"{"repo": "../b"}"#,
            r#"{"repo": 7}"#,
            "not json",
        ] {
            assert_eq!(repo_from_record(record), None, "{}", record);
        }
    }

    #[test]
    fn a_release_names_a_plain_tag() {
        assert_eq!(
            tag_from_release(br#"{"tag_name": "v0.3.0", "name": "0.3.0"}"#).as_deref(),
            Some("v0.3.0")
        );
        assert_eq!(tag_from_release(br#"{"tag_name": "v0.3.0/../x"}"#), None);
        assert_eq!(tag_from_release(br#"{"message": "Not Found"}"#), None);
        assert_eq!(tag_from_release(b""), None);
    }
}
