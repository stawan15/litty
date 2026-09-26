//! Self-update: a quiet check for a newer release, a signed download, and a swap when the app quits.
//! Network access goes through `curl` (no HTTP/TLS code in litty); releases are verified with an
//! ed25519 signature made by the release workflow, so a tampered download is refused.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const CHECK_EVERY: u64 = 3600;
/// Public half of the key that signs releases (`examples/sign.rs`).
const PUBLIC_KEY: [u8; 32] = [0x0b, 0xeb, 0xc3, 0x2a, 0xd0, 0xf1, 0x99, 0x1d, 0xe2, 0x03, 0x39, 0xd5, 0xf2, 0xc6, 0x9e, 0xf2, 0x8b, 0x27, 0xd0, 0xb1, 0x0c, 0x19, 0x4d, 0x16, 0xc7, 0xed, 0x3b, 0xf6, 0x75, 0x97, 0xef, 0xd0];

/// What the background threads report to the app.
pub enum Event {
    /// A newer release exists (version without the leading "v").
    Found(String),
    /// The new version was downloaded, verified and unpacked, ready to be swapped in on quit.
    Staged(String, PathBuf),
    /// The new version's system package (.deb or .rpm) was installed; it runs from the next launch.
    Installed(String),
    /// An explicit check found nothing newer.
    Current,
    Failed(String),
}

#[derive(Clone, PartialEq, Default)]
pub enum State {
    #[default]
    None,
    Available(String),
    Downloading(String),
    Staged(String, PathBuf),
    Installed(String),
    Failed(String),
}

fn base() -> String {
    std::env::var("LITTY_UPDATE_BASE").unwrap_or_else(|_| "https://github.com/stawan15/litty".into())
}

fn cache_dir() -> Option<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(cache.join("litty"))
}

pub fn enabled() -> bool {
    std::env::var_os("LITTY_NO_UPDATE_CHECK").is_none()
}

/// "1.2.3" → (1, 2, 3); anything after a '-' or '+' is ignored.
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim_start_matches('v').split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((it.next()??, it.next()??, it.next()??))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse_version(candidate), parse_version(current)), (Some(a), Some(b)) if a > b)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// State file: "last_check_epoch latest_version skipped_version" (versions may be empty as "-").
fn read_state() -> (u64, String, String) {
    let text = cache_dir().and_then(|d| std::fs::read_to_string(d.join("update-check")).ok()).unwrap_or_default();
    let mut it = text.split_whitespace();
    let ts = it.next().and_then(|t| t.parse().ok()).unwrap_or(0);
    let clean = |s: Option<&str>| s.filter(|s| *s != "-").unwrap_or("").to_string();
    (ts, clean(it.next()), clean(it.next()))
}

fn write_state(ts: u64, latest: &str, skipped: &str) {
    let Some(dir) = cache_dir() else { return };
    let dash = |s: &str| if s.is_empty() { "-".to_string() } else { s.to_string() };
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("update-check"), format!("{ts} {} {}\n", dash(latest), dash(skipped)));
}

/// Never offer `version` again.
pub fn skip(version: &str) {
    let (ts, latest, _) = read_state();
    write_state(ts, &latest, version);
}

/// The newest release tag, from the redirect of `<repo>/releases/latest` (no API, no rate limit).
fn fetch_latest() -> Option<String> {
    let out = Command::new("curl")
        .args(["-fsSLI", "--max-time", "10", "-o", "/dev/null", "-w", "%{url_effective}"])
        .arg(format!("{}/releases/latest", base()))
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let url = String::from_utf8(out.stdout).ok()?;
    let tag = url.trim().rsplit('/').next()?.trim_start_matches('v').to_string();
    parse_version(&tag).map(|_| tag)
}

/// Report a newer release, asking the network at most once an hour. `force` (the user asked) always
/// asks, also offers a skipped version, and reports when litty is current or GitHub can't be reached.
pub fn check(force: bool, report: impl FnOnce(Event)) {
    let (ts, mut latest, skipped) = read_state();
    if force || now().saturating_sub(ts) >= CHECK_EVERY {
        match fetch_latest() {
            Some(tag) => {
                latest = tag;
                write_state(now(), &latest, &skipped);
            }
            None if force => return report(Event::Failed("can't reach GitHub".into())),
            None => return,
        }
    }
    if is_newer(&latest, VERSION) && (force || latest != skipped) {
        report(Event::Found(latest));
    } else if force {
        report(Event::Current);
    }
}

fn exe() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|p| p.canonicalize().ok())
}

/// The enclosing `litty.app` when running from a macOS app bundle.
fn app_bundle() -> Option<PathBuf> {
    let app = exe()?.ancestors().nth(3)?.to_path_buf();
    app.extension().is_some_and(|e| e == "app").then_some(app)
}

/// The system package format ("deb" or "rpm") that owns this installation, when litty can upgrade
/// it through pkexec (which asks for the password in a desktop dialog).
fn package(exe: &Path) -> Option<&'static str> {
    if !exe.starts_with("/usr/") || !Path::new("/usr/bin/pkexec").exists() {
        return None;
    }
    if Path::new("/var/lib/dpkg/info/litty.list").exists() {
        Some("deb")
    } else if Command::new("rpm").arg("-qf").arg(exe).output().is_ok_and(|o| o.status.success()) {
        Some("rpm")
    } else {
        None
    }
}

/// Whether an update is a system package that `stage` installs right away.
pub fn installs_now() -> bool {
    exe().as_deref().and_then(package).is_some()
}

fn writable_dir(dir: &Path) -> bool {
    let probe = dir.join(".litty-write-test");
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(probe);
    ok
}

/// If a package manager owns this installation, the command that updates it; None when litty can
/// replace itself.
pub fn managed_by() -> Option<String> {
    let exe = exe()?;
    let path = exe.to_string_lossy();
    if cfg!(target_os = "macos") {
        let caskroom = ["/opt/homebrew/Caskroom/litty", "/usr/local/Caskroom/litty"].iter().any(|p| Path::new(p).exists());
        if caskroom {
            return Some("brew upgrade --cask litty".into());
        }
        let app = app_bundle()?;
        return (!writable_dir(app.parent()?)).then(|| "download the new dmg from GitHub".into());
    }
    if path.contains("/Cellar/") || path.contains("/homebrew/") {
        Some("brew upgrade litty".into())
    } else if path.starts_with("/nix/store") {
        Some("nix profile upgrade litty".into())
    } else if path.contains("/.cargo/bin/") {
        Some("cargo install litty-term".into())
    } else if package(&exe).is_some() {
        None
    } else if path.starts_with("/usr/") {
        Some("update litty with your package manager".into())
    } else {
        (!writable_dir(exe.parent()?)).then(|| "download the new release from GitHub".into())
    }
}

fn download(url: &str, to: &Path) -> Result<(), String> {
    let ok = Command::new("curl").args(["-fsSL", "--max-time", "600", "-o"]).arg(to).arg(url).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if ok { Ok(()) } else { Err(format!("download failed: {url}")) }
}

/// Check `file` against `file.sig` (raw ed25519 signature) with the built-in public key.
pub fn verify(file: &Path, sig: &Path) -> Result<(), String> {
    let data = std::fs::read(file).map_err(|e| e.to_string())?;
    let sig = std::fs::read(sig).map_err(|e| e.to_string())?;
    let sig = ed25519_compact::Signature::from_slice(&sig).map_err(|_| "bad signature file".to_string())?;
    ed25519_compact::PublicKey::new(PUBLIC_KEY).verify(data, &sig).map_err(|_| "signature check failed".to_string())
}

fn run(cmd: &mut Command) -> Result<(), String> {
    match cmd.stdout(Stdio::null()).stderr(Stdio::null()).status() {
        Ok(s) if s.success() => Ok(()),
        _ => Err(format!("{:?} failed", cmd.get_program())),
    }
}

/// Download, verify and unpack release `version` for `apply` to swap in on quit. A system package is
/// installed right away instead.
pub fn stage(version: &str) -> Result<Event, String> {
    let dir = cache_dir().ok_or("no cache directory")?.join("staged-update");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let release = format!("{}/releases/download/v{version}", base());
    let fetch = |name: &str| -> Result<PathBuf, String> {
        let path = dir.join(name);
        download(&format!("{release}/{name}"), &path)?;
        download(&format!("{release}/{name}.sig"), &dir.join(format!("{name}.sig")))?;
        verify(&path, &dir.join(format!("{name}.sig")))?;
        Ok(path)
    };
    if cfg!(target_os = "macos") {
        let dmg = fetch(&format!("litty-{version}-macos-universal.dmg"))?;
        let mnt = dir.join("mnt");
        std::fs::create_dir_all(&mnt).map_err(|e| e.to_string())?;
        run(Command::new("hdiutil").args(["attach", "-nobrowse", "-readonly", "-quiet", "-mountpoint"]).arg(&mnt).arg(&dmg))?;
        let copied = run(Command::new("ditto").arg(mnt.join("litty.app")).arg(dir.join("litty.app")));
        let _ = run(Command::new("hdiutil").args(["detach", "-quiet"]).arg(&mnt));
        copied?;
        Ok(Event::Staged(version.into(), dir.join("litty.app")))
    } else if let Some(kind) = exe().as_deref().and_then(package) {
        let arch = std::env::consts::ARCH;
        let (file, install) = if kind == "deb" {
            (format!("litty_{version}-1_{}.deb", if arch == "aarch64" { "arm64" } else { "amd64" }), ["dpkg", "-i"])
        } else {
            (format!("litty-{version}-1.{arch}.rpm"), ["rpm", "-U"])
        };
        let file = fetch(&file)?;
        run(Command::new("pkexec").args(install).arg(&file)).map_err(|_| "install cancelled".to_string())?;
        let _ = std::fs::remove_dir_all(&dir);
        Ok(Event::Installed(version.into()))
    } else {
        let arch = if std::env::consts::ARCH == "aarch64" { "aarch64" } else { "x86_64" };
        let name = format!("litty-{version}-{arch}-unknown-linux-gnu");
        let tar = fetch(&format!("{name}.tar.gz"))?;
        run(Command::new("tar").arg("-xzf").arg(&tar).arg("-C").arg(&dir))?;
        Ok(Event::Staged(version.into(), dir.join(&name).join("litty")))
    }
}

/// Swap the staged release in place of the running installation (called as the app exits).
pub fn apply(staged: &Path) -> Result<(), String> {
    let err = |e: std::io::Error| e.to_string();
    if cfg!(target_os = "macos") {
        let app = app_bundle().ok_or("not running from an app bundle")?;
        let parent = app.parent().ok_or("no parent directory")?;
        let (new, old) = (parent.join(".litty-update.app"), parent.join(".litty-old.app"));
        let _ = std::fs::remove_dir_all(&new);
        let _ = std::fs::remove_dir_all(&old);
        run(Command::new("ditto").arg(staged).arg(&new))?;
        std::fs::rename(&app, &old).map_err(err)?;
        if let Err(e) = std::fs::rename(&new, &app) {
            let _ = std::fs::rename(&old, &app);
            return Err(e.to_string());
        }
        let _ = std::fs::remove_dir_all(old);
    } else {
        let exe = exe().ok_or("unknown executable path")?;
        let tmp = exe.with_extension("new");
        std::fs::copy(staged, &tmp).map_err(err)?;
        std::fs::rename(&tmp, &exe).map_err(err)?;
    }
    if let Some(dir) = staged.ancestors().find(|p| p.file_name().is_some_and(|n| n == "staged-update")) {
        let _ = std::fs::remove_dir_all(dir);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("v1.0.0", "0.99.99"));
        assert!(!is_newer("0.2.1", "0.2.1"));
        assert!(!is_newer("0.2.0", "0.2.1"));
        assert!(!is_newer("garbage", "0.2.1"));
        assert!(is_newer("0.3.0-beta", "0.2.1"));
    }

    #[test]
    fn signatures_are_checked() {
        let dir = std::env::temp_dir().join(format!("litty-sig-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (file, sig) = (dir.join("f"), dir.join("f.sig"));
        std::fs::write(&file, b"hello").unwrap();
        std::fs::write(&sig, [0u8; 64]).unwrap();
        assert!(verify(&file, &sig).is_err(), "an all-zero signature must not verify");
        std::fs::write(&sig, [0u8; 10]).unwrap();
        assert!(verify(&file, &sig).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
