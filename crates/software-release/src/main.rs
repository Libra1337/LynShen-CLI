//! Builds and signs releases for the LynShen software library
//! (docs/software-library.md). The signing key stays on the release machine;
//! the CLI ships only its public half.
//!
//! software-release keygen <key-file>
//! software-release sign <key-file> <product> <version> <dir> <out-dir>
//! software-release verify <public-key> <manifest>

use base64::{engine::general_purpose::STANDARD, Engine};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read},
    path::Path,
    process::ExitCode,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["keygen", key] => keygen(Path::new(key)),
        ["sign", key, product, version, dir, out] => sign(
            Path::new(key),
            product,
            version,
            Path::new(dir),
            Path::new(out),
        ),
        ["verify", public, manifest] => {
            fs::read_to_string(manifest)
                .map_err(|error| error.to_string())
                .and_then(|text| verify(public, &text))
                .map(|release| println!("{}", release))
        }
        _ => Err("usage: software-release keygen <key> | sign <key> <product> <version> <dir> <out> | verify <public-key> <manifest>".to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("software-release: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Writes a new private key (base64 seed, mode 0600) and prints the public key.
fn keygen(path: &Path) -> Result<(), String> {
    if path.exists() {
        return Err(format!(
            "{} exists; refusing to overwrite a key",
            path.display()
        ));
    }
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|error| error.to_string())?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| error.to_string())?;
    io::Write::write_all(&mut file, format!("{}\n", STANDARD.encode(seed)).as_bytes())
        .map_err(|error| error.to_string())?;
    println!(
        "{}",
        STANDARD.encode(SigningKey::from_bytes(&seed).verifying_key().as_bytes())
    );
    Ok(())
}

fn read_key(path: &Path) -> Result<SigningKey, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let seed = STANDARD
        .decode(text.trim())
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .ok_or("the key file does not hold a 32-byte base64 seed")?;
    Ok(SigningKey::from_bytes(&seed))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

/// Hashes every file in `dir`, signs the release and writes
/// `<out>/<version>/<files>` plus `<out>/latest.json`.
fn sign(key: &Path, product: &str, version: &str, dir: &Path, out: &Path) -> Result<(), String> {
    if !safe_name(product) {
        return Err("invalid product name".to_string());
    }
    semver::Version::parse(version).map_err(|error| format!("version {version}: {error}"))?;
    let key = read_key(key)?;
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|error| format!("{}: {error}", dir.display()))?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .collect();
    entries.sort_by_key(|entry| entry.file_name());
    let target = out.join(version);
    fs::create_dir_all(&target).map_err(|error| error.to_string())?;
    let mut files = Vec::new();
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !safe_name(&name) {
            return Err(format!("invalid file name {name:?}"));
        }
        let size = entry.metadata().map_err(|error| error.to_string())?.len();
        files.push(json!({ "name": name, "sha256": sha256_file(&entry.path())?, "size": size }));
        fs::copy(entry.path(), target.join(&name)).map_err(|error| error.to_string())?;
    }
    if files.is_empty() {
        return Err(format!("{} holds no files", dir.display()));
    }
    let release = json!({ "product": product, "version": version, "files": files }).to_string();
    let signature = key.sign(release.as_bytes());
    let manifest = json!({
        "release": release,
        "signature": STANDARD.encode(signature.to_bytes()),
        "key": STANDARD.encode(key.verifying_key().as_bytes()),
    });
    let text = serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?;
    verify(&STANDARD.encode(key.verifying_key().as_bytes()), &text)?;
    fs::write(out.join("latest.json"), format!("{text}\n")).map_err(|error| error.to_string())?;
    println!("signed {product} {version}: {} files", files.len());
    Ok(())
}

/// Checks a manifest's signature against `public` and returns its release.
fn verify(public: &str, manifest: &str) -> Result<Value, String> {
    let envelope: Value = serde_json::from_str(manifest).map_err(|error| error.to_string())?;
    let release = envelope["release"].as_str().ok_or("no signed release")?;
    let signature = STANDARD
        .decode(envelope["signature"].as_str().unwrap_or_default())
        .ok()
        .and_then(|bytes| Signature::from_slice(&bytes).ok())
        .ok_or("malformed signature")?;
    let key = STANDARD
        .decode(public.trim())
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .and_then(|bytes| VerifyingKey::from_bytes(&bytes).ok())
        .ok_or("malformed public key")?;
    key.verify(release.as_bytes(), &signature)
        .map_err(|_| "signature does not verify".to_string())?;
    serde_json::from_str(release).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("software-release-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_signed_release_verifies_and_a_changed_one_does_not() {
        let dir = temp("sign");
        let key = dir.join("key");
        keygen(&key).unwrap();
        let public = STANDARD.encode(read_key(&key).unwrap().verifying_key().as_bytes());
        let files = dir.join("files");
        fs::create_dir_all(&files).unwrap();
        fs::write(files.join("lynshen-aarch64-apple-darwin"), b"binary").unwrap();
        let out = dir.join("out");
        sign(&key, "cli", "1.2.3", &files, &out).unwrap();

        let manifest = fs::read_to_string(out.join("latest.json")).unwrap();
        let release = verify(&public, &manifest).unwrap();
        assert_eq!(release["version"], "1.2.3");
        assert_eq!(
            release["files"][0]["sha256"],
            "9a3a45d01531a20e89ac6ae10b0b0beb0492acd7216a368aa062d1a5fecaf9cd"
        );
        assert!(out.join("1.2.3/lynshen-aarch64-apple-darwin").is_file());

        let tampered = manifest.replace("1.2.3", "9.9.9");
        assert!(verify(&public, &tampered).is_err());
        assert!(
            keygen(&key).is_err(),
            "an existing key is never overwritten"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
