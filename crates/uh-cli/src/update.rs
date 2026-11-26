//! A newer release: looked for at most once a day when the terminal UI
//! opens, told there, and installed by `unharnessed update`, which runs
//! the README's installer again.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The latest release (GitHub's: neither a draft nor a pre-release).
const LATEST: &str = "https://api.github.com/repos/unharnessed-agent/Unharnessed/releases/latest";

/// The installer the README runs on this platform.
#[cfg(not(windows))]
const INSTALLER: &str =
    "https://raw.githubusercontent.com/unharnessed-agent/Unharnessed/main/install.sh";
#[cfg(windows)]
const INSTALLER: &str =
    "https://raw.githubusercontent.com/unharnessed-agent/Unharnessed/main/install.ps1";

/// How long a look holds before a launch looks again.
const FRESH_FOR: Duration = Duration::from_hours(24);

/// How long a look waits for GitHub.
const PATIENCE: Duration = Duration::from_secs(10);

/// The last look, in the agent's folder: the latest version and when.
const LOOKED: &str = "update-check.json";

/// What the terminal UI tells first: a newer release, as the last look
/// saw it. A look older than a day starts another, in the background,
/// for the next launch. Nothing offline.
pub fn news(agent_dir: &Path) -> Option<String> {
    if crate::config::is_truthy_env_flag(std::env::var(crate::config::ENV_OFFLINE).ok().as_deref())
    {
        return None;
    }
    let file = agent_dir.join(LOOKED);
    let looked = read(&file);
    let now = seconds();
    let fresh = looked
        .as_ref()
        .is_some_and(|(at, _)| *at <= now && now - at < FRESH_FOR.as_secs());
    if !fresh {
        std::thread::spawn(move || {
            if let Ok(latest) = latest() {
                write(&file, &latest);
            }
        });
    }
    let (_, latest) = looked?;
    let current = crate::config::version();
    newer(&latest, current).then(|| {
        format!("Update available: {current} → {latest}. Run unharnessed update to install it.")
    })
}

/// `unharnessed update`: the installer again, when a newer release is out.
pub fn run(args: &[String]) -> i32 {
    if let Some(arg) = args.first() {
        eprintln!("Error: update takes no arguments, got {arg}");
        return 1;
    }
    let current = crate::config::version();
    match latest() {
        Ok(latest) => {
            write(&crate::config::get_agent_dir().join(LOOKED), &latest);
            if !newer(&latest, current) {
                println!("Unharnessed {current} is up to date.");
                return 0;
            }
            println!("Updating Unharnessed {current} to {latest}…");
        }
        Err(error) => {
            eprintln!(
                "Could not look for the latest release ({error}): running the installer anyway."
            );
        }
    }
    match install() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    }
}

/// The installer, downloaded and run with this terminal.
fn install() -> Result<i32, String> {
    let script = block_on(async {
        let response = client()?
            .get(INSTALLER)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| said(&error))?;
        response.text().await.map_err(|error| error.to_string())
    })
    .map_err(|error| format!("cannot download the installer {INSTALLER}: {error}"))?;
    let name = INSTALLER.rsplit('/').next().unwrap_or("install");
    let file = std::env::temp_dir().join(format!("unharnessed-{}-{name}", std::process::id()));
    std::fs::write(&file, script)
        .map_err(|error| format!("cannot write the installer to {}: {error}", file.display()))?;
    let status = installer(&file).status();
    let _ = std::fs::remove_file(&file);
    let status = status.map_err(|error| format!("cannot run the installer: {error}"))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(not(windows))]
fn installer(file: &Path) -> Command {
    let mut command = Command::new("sh");
    command.arg(file);
    command
}

/// Windows PowerShell is on every Windows; PowerShell 7 when it is there.
#[cfg(windows)]
fn installer(file: &Path) -> Command {
    let powershell = uh_core::platform::KernelShells::resolve()
        .powershell
        .unwrap_or_else(|| "powershell.exe".to_string());
    let mut command = Command::new(powershell);
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(file);
    command
}

/// The latest release's version, from its tag (`vscode-v0.0.9`).
fn latest() -> Result<String, String> {
    let tag = block_on(async {
        let response = client()?
            .get(LATEST)
            .header("accept", "application/vnd.github+json")
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| said(&error))?;
        let body = response.text().await.map_err(|error| error.to_string())?;
        let release: serde_json::Value =
            serde_json::from_str(&body).map_err(|error| error.to_string())?;
        Ok(release["tag_name"].as_str().unwrap_or_default().to_string())
    })?;
    let version = tag.strip_prefix("vscode-v").unwrap_or(&tag);
    match parse(version) {
        Some(_) => Ok(version.to_string()),
        None => Err(format!(
            "the latest release's tag names no version: '{tag}'"
        )),
    }
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(PATIENCE)
        // GitHub's API answers no request without one.
        .user_agent(format!("unharnessed/{}", crate::config::version()))
        .build()
        .map_err(|error| error.to_string())
}

/// The error with its causes: reqwest's own says only that a request failed.
fn said(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut cause = error.source();
    while let Some(error) = cause {
        text = format!("{text}: {error}");
        cause = error.source();
    }
    text
}

fn block_on<T>(work: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?
        .block_on(work)
}

fn seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn read(file: &Path) -> Option<(u64, String)> {
    let text = std::fs::read_to_string(file).ok()?;
    let looked: serde_json::Value = serde_json::from_str(&text).ok()?;
    let at = looked["checkedAt"].as_u64()?;
    let latest = looked["latest"].as_str()?;
    Some((at, latest.to_string()))
}

/// Written whole or not at all: another launch may read it meanwhile.
fn write(file: &Path, latest: &str) {
    let text = serde_json::json!({ "checkedAt": seconds(), "latest": latest }).to_string();
    let part = PathBuf::from(format!("{}.{}", file.display(), std::process::id()));
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::write(&part, text).is_ok() && std::fs::rename(&part, file).is_err() {
        let _ = std::fs::remove_file(&part);
    }
}

/// `1.2.3`'s numbers; `None` for anything else (a pre-release among them).
fn parse(version: &str) -> Option<Vec<u64>> {
    version
        .split('.')
        .map(|part| {
            let digits = !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
            digits.then(|| part.parse().ok()).flatten()
        })
        .collect()
}

/// `latest` comes after `current`; a version either cannot be read as is not.
fn newer(latest: &str, current: &str) -> bool {
    let (Some(latest), Some(current)) = (parse(latest), parse(current)) else {
        return false;
    };
    let length = latest.len().max(current.len());
    let at = |numbers: &[u64], index: usize| numbers.get(index).copied().unwrap_or(0);
    (0..length)
        .map(|index| at(&latest, index).cmp(&at(&current, index)))
        .find(|order| order.is_ne())
        .is_some_and(std::cmp::Ordering::is_gt)
}

#[cfg(test)]
mod tests {
    use super::newer;

    // How the comparison can go wrong: numbers compared as text, an equal
    // or older release told (a dev build is ahead of every release),
    // versions of different lengths, a pre-release or a tag that is not a
    // version taken for one.
    #[test]
    fn a_newer_release_is_told_and_nothing_else() {
        let cases = [
            ("0.0.9", "0.0.8", true),
            ("0.0.10", "0.0.9", true),
            ("0.1.0", "0.0.99", true),
            ("1.0.0", "0.9.8", true),
            ("0.1", "0.0.9", true),
            ("0.0.8", "0.0.8", false),
            ("0.1", "0.1.0", false),
            ("0.0.8", "0.9.8", false),
            ("0.0.9", "0.0.10", false),
            ("0.0.9-rc.1", "0.0.8", false),
            ("0.0.9", "0.0.8-dev", false),
            ("+1.0.0", "0.0.8", false),
            ("", "0.0.8", false),
            ("0..9", "0.0.8", false),
            ("latest", "0.0.8", false),
        ];
        for (latest, current, told) in cases {
            assert_eq!(newer(latest, current), told, "{latest} after {current}");
        }
    }
}
