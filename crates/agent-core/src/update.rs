use base64::{engine::general_purpose::STANDARD, Engine};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use semver::Version;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

const UPDATE_COMMAND: &str = "lynshen update";
/// The LynShen software library: one signed manifest per product, files by
/// version (docs/software-library.md).
const SOFTWARE_URL: &str = "https://software.lynshen.org/cli/latest.json";
/// Where people download the CLI by hand.
const DOWNLOAD_PAGE: &str = "https://www.lynshen.org/download";
/// Ed25519 key that signs `latest.json`. Its private half signs releases
/// offline; a manifest under any other key is refused.
const MANIFEST_PUBLIC_KEY: &str = "rNaDD3lDFhpKdEGDOZdTHGBW9uEoQJ5jaQeTYrhz0tQ=";
const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub struct UpdateNotice {
    pub current_version: String,
    pub latest_version: String,
    /// Already installed in the background; active from the next start.
    pub installed: bool,
}

impl UpdateNotice {
    pub fn message(&self) -> String {
        if self.installed {
            format!(
                "LynShen {} installed; restart lynshen to use it (now {})",
                self.latest_version, self.current_version
            )
        } else {
            format!(
                "update available: LynShen {} -> {}, run {}",
                self.current_version, self.latest_version, UPDATE_COMMAND
            )
        }
    }
}

/// Checks the software library in the background. A release binary with
/// `auto_update` on installs a newer version itself (it runs from the next
/// start); otherwise the notice tells the user to run `lynshen update`.
pub fn spawn_update_check(
    current_version: &'static str,
    auto_update: bool,
) -> Receiver<UpdateNotice> {
    let (tx, rx) = mpsc::channel();
    // LynShen Desktop updates the copy it manages; no notice for it.
    let channel = install_channel();
    if channel == InstallChannel::Desktop {
        return rx;
    }
    thread::spawn(move || {
        let Ok(release) = latest_release(UPDATE_CHECK_TIMEOUT) else {
            return;
        };
        if !is_newer_version(current_version, &release.version) {
            return;
        }
        let installed = auto_update
            && channel == InstallChannel::Other
            && cfg!(not(debug_assertions))
            && self_update(&release).is_ok();
        let _ = tx.send(UpdateNotice {
            current_version: current_version.to_string(),
            latest_version: release.version,
            installed,
        });
    });
    rx
}

/// One release of the CLI: its version and downloadable files.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    /// Hex sha256 from the signed manifest; a download must match it.
    pub sha256: String,
}

impl Release {
    /// This platform's binary (`lynshen-<target>`, `.exe` on Windows).
    pub fn binary(&self) -> Option<&Asset> {
        let name = binary_name()?;
        self.assets.iter().find(|asset| asset.name == name)
    }
}

/// The release asset name of this platform's binary, by target triple.
fn binary_name() -> Option<String> {
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        _ => return None,
    };
    Some(if cfg!(windows) {
        format!("lynshen-{target}.exe")
    } else {
        format!("lynshen-{target}")
    })
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout_read(timeout)
        .build()
}

/// The newest CLI version in the software library.
pub fn latest_cli_version() -> Result<String, String> {
    latest_release(UPDATE_CHECK_TIMEOUT).map(|release| release.version)
}

/// The newest release in the software library, its manifest signature checked.
pub fn latest_release(timeout: Duration) -> Result<Release, String> {
    release_at(SOFTWARE_URL, timeout)
}

fn release_at(manifest_url: &str, timeout: Duration) -> Result<Release, String> {
    let manifest = agent(timeout)
        .get(manifest_url)
        .set("Accept", "application/json")
        .set("User-Agent", "lynshen-cli")
        .call()
        .map_err(|error| error.to_string())?
        .into_string()
        .map_err(|error| error.to_string())?;
    parse_signed_release(&manifest, manifest_url, MANIFEST_PUBLIC_KEY)
}

/// `latest.json`: `{"release": {...}, "signature": "<base64>"}`, where the
/// signature covers the exact bytes of the `release` value as served. The
/// release names files relative to the manifest's directory.
fn parse_signed_release(
    manifest: &str,
    manifest_url: &str,
    public_key: &str,
) -> Result<Release, String> {
    let envelope: Value = serde_json::from_str(manifest).map_err(|error| error.to_string())?;
    let signed = envelope["release"]
        .as_str()
        .ok_or("manifest has no signed release")?;
    let signature = STANDARD
        .decode(text(&envelope["signature"]))
        .ok()
        .and_then(|bytes| Signature::from_slice(&bytes).ok())
        .ok_or("manifest signature is malformed")?;
    let key = STANDARD
        .decode(public_key)
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .and_then(|bytes| VerifyingKey::from_bytes(&bytes).ok())
        .ok_or("no valid update key in this build")?;
    key.verify(signed.as_bytes(), &signature)
        .map_err(|_| "manifest signature does not verify".to_string())?;
    let release: Value = serde_json::from_str(signed).map_err(|error| error.to_string())?;
    if release["product"] != "cli" {
        return Err("manifest is not for the CLI".to_string());
    }
    let base = manifest_url
        .rsplit_once('/')
        .map_or(manifest_url, |(dir, _)| dir);
    parse_release(&release, base)
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

/// The signed release: a version and one entry per platform binary, each
/// with its sha256. Files live in `<base>/<version>/`; names cannot leave it.
fn parse_release(value: &Value, base: &str) -> Result<Release, String> {
    let version = text(&value["version"]).to_string();
    if Version::parse(&version).is_err() {
        return Err("manifest has no valid version".to_string());
    }
    let mut assets = Vec::new();
    for file in value["files"].as_array().into_iter().flatten() {
        let name = text(&file["name"]);
        let sha256 = text(&file["sha256"]).to_ascii_lowercase();
        let safe_name = !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
            && !name.starts_with('.');
        if !safe_name || sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("manifest lists an invalid file {name:?}"));
        }
        assets.push(Asset {
            url: format!("{base}/{version}/{name}"),
            name: name.to_string(),
            sha256,
        });
    }
    Ok(Release { version, assets })
}

pub fn is_newer_version(current_version: &str, latest_version: &str) -> bool {
    let Ok(current) = Version::parse(current_version.trim_start_matches('v')) else {
        return false;
    };
    let Ok(latest) = Version::parse(latest_version.trim_start_matches('v')) else {
        return false;
    };
    latest > current
}

/// How the running binary was installed, detected from its own path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallChannel {
    /// Inside an npm global `node_modules/@lynshen/` tree — `npm i -g` manages it.
    Npm,
    /// The copy LynShen Desktop keeps in `~/.lynshen/bin` — it updates with the app.
    Desktop,
    /// Anything else: a release binary, cargo install, dev build.
    Other,
}

pub fn install_channel() -> InstallChannel {
    let path = std::env::current_exe()
        .map(|path| fs::canonicalize(&path).unwrap_or(path))
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let desktop = crate::config::lynshen_dir()
        .map(|dir| fs::canonicalize(dir.join("bin")).unwrap_or(dir.join("bin")))
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default();
    channel_for_path(&path, &desktop)
}

fn channel_for_path(path: &str, desktop_bin: &str) -> InstallChannel {
    let path = path.replace('\\', "/");
    let desktop_bin = desktop_bin.replace('\\', "/");
    if path.contains("node_modules/@lynshen/") {
        InstallChannel::Npm
    } else if !desktop_bin.is_empty()
        && path.starts_with(&format!("{}/", desktop_bin.trim_end_matches('/')))
    {
        InstallChannel::Desktop
    } else {
        InstallChannel::Other
    }
}

/// `lynshen update` for a binary installed from a release: downloads this
/// platform's binary of `release` from the software library, checks its
/// sha256 and version, and puts it in place of the running one. The new
/// version runs from the next start.
pub fn self_update(release: &Release) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let exe = fs::canonicalize(&exe).unwrap_or(exe);
    let name = binary_name().ok_or("no release binary for this platform")?;
    let asset = release
        .binary()
        .ok_or_else(|| format!("release {} has no {name}", release.version))?;
    let partial = exe.with_file_name(format!(".{name}.download"));
    let result = download(asset, &partial)
        .and_then(|()| check_binary(&partial, &release.version))
        .and_then(|()| replace_binary(&partial, &exe));
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result.map(|()| format!("updated to {}: restart lynshen to use it", release.version))
}

/// Streams `asset` to `dest` and checks its sha256.
fn download(asset: &Asset, dest: &Path) -> Result<(), String> {
    let response = agent(Duration::from_secs(15))
        .get(&asset.url)
        .set("User-Agent", "lynshen-cli")
        .call()
        .map_err(|error| error.to_string())?;
    let mut reader = response.into_reader();
    let mut file =
        fs::File::create(dest).map_err(|error| format!("{}: {error}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .map_err(|error| error.to_string())?;
        hasher.update(&buffer[..read]);
    }
    file.sync_all().map_err(|error| error.to_string())?;
    let digest = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if asset.sha256.eq_ignore_ascii_case(&digest) {
        Ok(())
    } else {
        Err(format!("checksum mismatch for {}", asset.name))
    }
}

/// The downloaded binary runs and reports the expected version.
fn check_binary(path: &Path, version: &str) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .map_err(|error| error.to_string())?;
    }
    let output = Command::new(path)
        .arg("--version")
        .output()
        .map_err(|error| format!("the downloaded binary does not run: {error}"))?;
    let reported = String::from_utf8_lossy(&output.stdout);
    if reported.split_whitespace().last() == Some(version) {
        Ok(())
    } else {
        Err(format!("the downloaded binary reports {}", reported.trim()))
    }
}

/// Puts `new` where `exe` is. Windows keeps a running executable locked but
/// lets it be renamed, so the old one moves aside first.
fn replace_binary(new: &Path, exe: &Path) -> Result<(), String> {
    let denied = |error: std::io::Error| {
        format!(
            "cannot replace {} ({error}); download it from {DOWNLOAD_PAGE}",
            exe.display()
        )
    };
    if cfg!(windows) {
        let old: PathBuf = exe.with_extension("old.exe");
        let _ = fs::remove_file(&old);
        fs::rename(exe, &old).map_err(denied)?;
        if let Err(error) = fs::rename(new, exe) {
            let _ = fs::rename(&old, exe);
            return Err(denied(error));
        }
        Ok(())
    } else {
        fs::rename(new, exe).map_err(denied)
    }
}

/// Runs `npm i -g @lynshen/cli@latest` for an npm-installed binary.
///
/// On Unix the foreground npm replaces the package files while this process
/// keeps running; the new version takes effect on the next launch. On Windows
/// the running executable is locked, so a detached helper waits for this
/// process to exit before running npm — its result is not visible here.
pub fn run_npm_update() -> Result<String, String> {
    #[cfg(windows)]
    {
        use std::{os::windows::process::CommandExt, process::Stdio};
        // DETACHED_PROCESS | CREATE_NO_WINDOW
        const FLAGS: u32 = 0x00000008 | 0x08000000;
        Command::new("cmd")
            .args([
                "/C",
                "timeout /t 2 /nobreak >nul && npm i -g @lynshen/cli@latest",
            ])
            .creation_flags(FLAGS)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("failed to start the updater: {error}"))?;
        Ok("update scheduled: npm i -g runs after lynshen exits; check `lynshen --version` in a few seconds".to_string())
    }
    #[cfg(not(windows))]
    {
        let status = Command::new("npm")
            .args(["i", "-g", "@lynshen/cli@latest"])
            .status()
            .map_err(|error| format!("failed to run npm (is it on PATH?): {error}"))?;
        if status.success() {
            Ok("updated: restart lynshen to use the new version".to_string())
        } else {
            Err(format!("npm i -g failed ({status})"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_semver_versions() {
        assert!(is_newer_version("0.1.9", "0.1.10"));
        assert!(is_newer_version("v0.1.9", "v0.2.0"));
        assert!(!is_newer_version("0.1.10", "0.1.9"));
        assert!(!is_newer_version("0.1.10", "0.1.10"));
        assert!(!is_newer_version("0.1.10", "not-a-version"));
    }

    #[test]
    fn notice_points_at_lynshen_update() {
        let mut notice = UpdateNotice {
            current_version: "0.1.3".to_string(),
            latest_version: "0.1.4".to_string(),
            installed: false,
        };
        assert!(notice.message().contains("lynshen update"));
        notice.installed = true;
        assert!(notice.message().contains("restart lynshen"));
    }

    #[test]
    fn detects_install_channels_across_platforms() {
        let desktop = "/home/x/.lynshen/bin";
        assert_eq!(
            channel_for_path(
                "/usr/local/lib/node_modules/@lynshen/cli-darwin-arm64/bin/lynshen",
                desktop
            ),
            InstallChannel::Npm
        );
        assert_eq!(
            channel_for_path(
                "C:\\Users\\x\\AppData\\Roaming\\npm\\node_modules\\@lynshen\\cli-win32-x64\\bin\\lynshen.exe",
                desktop
            ),
            InstallChannel::Npm
        );
        assert_eq!(
            channel_for_path("/home/x/.lynshen/bin/lynshen", desktop),
            InstallChannel::Desktop
        );
        assert_eq!(
            channel_for_path(
                "C:\\Users\\x\\.lynshen\\bin\\lynshen.exe",
                "C:\\Users\\x\\.lynshen\\bin"
            ),
            InstallChannel::Desktop
        );
        assert_eq!(
            channel_for_path("/home/x/.lynshen/binaries/lynshen", desktop),
            InstallChannel::Other
        );
        assert_eq!(
            channel_for_path("/home/x/bin/lynshen", desktop),
            InstallChannel::Other
        );
        assert_eq!(
            channel_for_path("/repo/target/debug/lynshen", desktop),
            InstallChannel::Other
        );
    }

    /// A manifest signed with a throwaway key, as software-release writes it.
    fn signed(release: &Value) -> (String, String) {
        use ed25519_dalek::{Signer, SigningKey};
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let release = release.to_string();
        let manifest = serde_json::json!({
            "release": release,
            "signature": STANDARD.encode(key.sign(release.as_bytes()).to_bytes()),
        })
        .to_string();
        (manifest, STANDARD.encode(key.verifying_key().as_bytes()))
    }

    #[test]
    fn reads_a_signed_release_from_the_software_library() {
        let sha = "9a3a45d01531a20e89ac6ae10b0b0beb0492acd7216a368aa062d1a5fecaf9cd";
        let release = serde_json::json!({
            "product": "cli",
            "version": "0.4.9",
            "files": [{ "name": "lynshen-aarch64-apple-darwin", "sha256": sha }],
        });
        let (manifest, key) = signed(&release);
        let parsed = parse_signed_release(&manifest, SOFTWARE_URL, &key).unwrap();
        assert_eq!(parsed.version, "0.4.9");
        assert_eq!(
            parsed.assets[0].url,
            "https://software.lynshen.org/cli/0.4.9/lynshen-aarch64-apple-darwin"
        );
        assert_eq!(parsed.assets[0].sha256, sha);
    }

    #[test]
    fn refuses_unsigned_tampered_or_unsafe_manifests() {
        let sha = "9a3a45d01531a20e89ac6ae10b0b0beb0492acd7216a368aa062d1a5fecaf9cd";
        let release = serde_json::json!({
            "product": "cli", "version": "0.4.9",
            "files": [{ "name": "lynshen-x86_64-unknown-linux-gnu", "sha256": sha }],
        });
        let (manifest, key) = signed(&release);
        let tampered = manifest.replace("0.4.9", "9.9.9");
        assert!(parse_signed_release(&tampered, SOFTWARE_URL, &key).is_err());
        assert!(parse_signed_release(&manifest, SOFTWARE_URL, MANIFEST_PUBLIC_KEY).is_err());
        let unsigned = serde_json::json!({ "release": release.to_string() }).to_string();
        assert!(parse_signed_release(&unsigned, SOFTWARE_URL, &key).is_err());
        for name in ["../lynshen", ".hidden", "a/b"] {
            let bad = serde_json::json!({
                "product": "cli", "version": "0.4.9",
                "files": [{ "name": name, "sha256": sha }],
            });
            let (manifest, key) = signed(&bad);
            assert!(
                parse_signed_release(&manifest, SOFTWARE_URL, &key).is_err(),
                "{name}"
            );
        }
        let desktop = serde_json::json!({ "product": "desktop", "version": "0.4.9", "files": [] });
        let (manifest, key) = signed(&desktop);
        assert!(parse_signed_release(&manifest, SOFTWARE_URL, &key).is_err());
    }

    #[test]
    fn the_built_in_update_key_is_valid() {
        let bytes = STANDARD.decode(MANIFEST_PUBLIC_KEY).unwrap();
        let bytes = <[u8; 32]>::try_from(bytes).unwrap();
        assert!(VerifyingKey::from_bytes(&bytes).is_ok());
    }

    /// Serves `body` once over HTTP on a loopback port; returns its URL.
    fn serve_once(body: &'static [u8]) -> String {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/lynshen", listener.local_addr().unwrap());
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap() > 2 {
                line.clear();
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
        });
        url
    }

    /// Serves files from `root` over loopback HTTP until the test ends.
    fn serve_dir(root: PathBuf) -> String {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap() > 2 {
                    line.clear();
                }
                let path = request
                    .split(' ')
                    .nth(1)
                    .unwrap_or("/")
                    .trim_start_matches('/');
                let (status, body) = match fs::read(root.join(path)) {
                    Ok(body) => ("200 OK", body),
                    Err(_) => ("404 Not Found", Vec::new()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        url
    }

    #[test]
    fn fetches_verifies_and_downloads_a_release_from_a_library() {
        use ed25519_dalek::{Signer, SigningKey};
        let root = std::env::temp_dir().join(format!("lynshen-library-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("cli/0.4.9")).unwrap();
        fs::write(root.join("cli/0.4.9/lynshen-test"), b"binary").unwrap();
        let key = SigningKey::from_bytes(&[9u8; 32]);
        let release = serde_json::json!({
            "product": "cli", "version": "0.4.9",
            "files": [{ "name": "lynshen-test",
                        "sha256": "9a3a45d01531a20e89ac6ae10b0b0beb0492acd7216a368aa062d1a5fecaf9cd" }],
        })
        .to_string();
        let manifest = serde_json::json!({
            "release": release,
            "signature": STANDARD.encode(key.sign(release.as_bytes()).to_bytes()),
        });
        fs::write(root.join("cli/latest.json"), manifest.to_string()).unwrap();
        let url = format!("{}/cli/latest.json", serve_dir(root.clone()));
        let public = STANDARD.encode(key.verifying_key().as_bytes());

        let text = agent(Duration::from_secs(5))
            .get(&url)
            .call()
            .unwrap()
            .into_string()
            .unwrap();
        let parsed = parse_signed_release(&text, &url, &public).unwrap();
        assert_eq!(parsed.version, "0.4.9");
        let dest = root.join("downloaded");
        download(&parsed.assets[0], &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"binary");
        // The built-in key refuses a manifest signed by anyone else.
        assert!(release_at(&url, Duration::from_secs(5)).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_download_is_checked_against_its_checksum() {
        let dir = std::env::temp_dir().join(format!("lynshen-update-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("lynshen");
        // sha256("binary")
        let good = "9a3a45d01531a20e89ac6ae10b0b0beb0492acd7216a368aa062d1a5fecaf9cd";
        let asset = |url: String, sha: &str| Asset {
            name: "lynshen".into(),
            url,
            sha256: sha.into(),
        };
        download(&asset(serve_once(b"binary"), good), &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"binary");
        let error = download(&asset(serve_once(b"tampered"), good), &dest).unwrap_err();
        assert!(error.contains("checksum mismatch"), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }
}
