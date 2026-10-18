//! Plugin distribution: signed archives and the installed lockfile.
//!
//! - `ka-agent plugin keygen` makes one ed25519 keypair per instance under
//!   `<data>/plugins/keys/` (0600). The public half is what vendors hand out.
//! - `plugin pack` builds a `.kplug` tar of the plugin directory and signs
//!   the exact archive bytes; the signature ships beside the archive.
//! - `plugin install` verifies the signature against every trusted key,
//!   checks the TOFU pin (the key that signed the first install of an id is
//!   the only key allowed to sign its updates), unpacks under
//!   `<data>/plugins/<id>/`, and records the content digest in the lockfile.
//!   Any later mismatch (tampering with installed files) disables the plugin
//!   at load with a visible reason.

use crate::manifest::{Manifest, PLUGIN_API};
use ed25519_dalek::{Signature, Signer, SigningKey as DalekKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const ARCHIVE_EXT: &str = "kplug";
pub const MANIFEST_NAME: &str = "manifest.toml";

#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("io {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0}")]
    Other(String),
}

impl From<std::io::Error> for PackError {
    fn from(source: std::io::Error) -> Self {
        PackError::Other(source.to_string())
    }
}

fn io(path: impl AsRef<Path>, source: std::io::Error) -> PackError {
    PackError::Io {
        path: path.as_ref().to_path_buf(),
        source,
    }
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// An ed25519 keypair for signing plugin archives.
pub struct SigningKey {
    inner: DalekKey,
}

/// The trusted public keys of an instance (`<data>/plugins/keys/*.pub`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TrustStore {
    /// hex verifying-key → label.
    pub keys: BTreeMap<String, String>,
}

impl SigningKey {
    /// Parse a 64-hex-byte seed.
    pub fn from_seed_hex(seed: &str) -> Result<Self, PackError> {
        let bytes = hex32(seed)?;
        Ok(Self {
            inner: DalekKey::from_bytes(&bytes),
        })
    }

    /// Generate from OS entropy.
    pub fn generate() -> Result<Self, PackError> {
        use rand_core::RngCore;
        let mut seed = [0u8; 32];
        rand_entropy().fill_bytes(&mut seed);
        Ok(Self {
            inner: DalekKey::from_bytes(&seed),
        })
    }

    pub fn seed_hex(&self) -> String {
        hex(&self.inner.to_bytes())
    }

    pub fn public_hex(&self) -> String {
        hex(self.inner.verifying_key().as_bytes())
    }

    fn sign(&self, message: &[u8]) -> String {
        let sig = self.inner.sign(message);
        hex(&sig.to_bytes())
    }
}

fn rand_entropy() -> impl rand_core::CryptoRngCore {
    rand_core::OsRng
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Result<Vec<u8>, PackError> {
    if !s.len().is_multiple_of(2) || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(PackError::Other(format!("bad hex (len {})", s.len())));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| PackError::Other(e.to_string())))
        .collect()
}

fn hex32(s: &str) -> Result<[u8; 32], PackError> {
    let v = hex_decode(s)?;
    v.try_into()
        .map_err(|_| PackError::Other("expected 32 bytes of hex".into()))
}

fn hex64(s: &str) -> Result<[u8; 64], PackError> {
    let v = hex_decode(s)?;
    v.try_into()
        .map_err(|_| PackError::Other("expected 64 bytes of hex".into()))
}

// ---------------------------------------------------------------------------
// Archives
// ---------------------------------------------------------------------------

/// A packed plugin: the tar bytes plus its detached ed25519 signature.
#[derive(Debug, Clone)]
pub struct SignedArchive {
    pub bytes: Vec<u8>,
    pub signature: String,
}

/// Build and sign an archive of `dir` (which must contain `manifest.toml`).
pub fn pack(dir: &Path, key: &SigningKey) -> Result<SignedArchive, PackError> {
    let manifest = Manifest::load(&dir.join(MANIFEST_NAME))
        .map_err(|e| PackError::Other(format!("manifest: {e}")))?;
    if manifest.plugin.api != PLUGIN_API {
        return Err(PackError::Other(format!(
            "manifest api {} ≠ host {PLUGIN_API}",
            manifest.plugin.api
        )));
    }
    let mut bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut bytes);
        append_dir(&mut builder, dir, "")?;
        builder.finish()?;
    }
    let signature = key.sign(&bytes);
    Ok(SignedArchive { bytes, signature })
}

fn append_dir(
    builder: &mut tar::Builder<&mut Vec<u8>>,
    dir: &Path,
    prefix: &str,
) -> Result<(), PackError> {
    let entries = std::fs::read_dir(dir).map_err(|e| io(dir, e))?;
    let mut names: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    names.sort();
    for path in names {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| PackError::Other(format!("non-utf8 name in {}", dir.display())))?;
        let rel = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        // Deterministic archives: skip editor droppings and caches.
        if name == ".git" || name.ends_with('~') {
            continue;
        }
        if path.is_dir() {
            append_dir(builder, &path, &rel)?;
        } else {
            let mut file = std::fs::File::open(&path).map_err(|e| io(&path, e))?;
            let meta = file.metadata().map_err(|e| io(&path, e))?;
            let mut header = tar::Header::new_gnu();
            header.set_size(meta.len());
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, &rel, &mut file)
                .map_err(|e| PackError::Other(format!("append {rel}: {e}")))?;
            let _ = file.read(&mut [0u8; 8]); // keep Read imported for clarity
        }
    }
    Ok(())
}

/// The manifest and every file path inside an archive, without unpacking to
/// disk.
pub fn archive_manifest(bytes: &[u8]) -> Result<Manifest, PackError> {
    let mut reader = std::io::Cursor::new(bytes);
    let mut archive = tar::Archive::new(&mut reader);
    for entry in archive
        .entries()
        .map_err(|e| PackError::Other(format!("tar: {e}")))?
    {
        let mut entry = entry.map_err(|e| PackError::Other(format!("tar: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| PackError::Other(format!("tar: {e}")))?
            .to_path_buf();
        if path == Path::new(MANIFEST_NAME) {
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .map_err(|_e| PackError::Other("manifest read".into()))?;
            return Manifest::parse(&text).map_err(|e| PackError::Other(format!("manifest: {e}")));
        }
    }
    Err(PackError::Other(format!(
        "archive has no {MANIFEST_NAME} at its root"
    )))
}

fn verify_signature(bytes: &[u8], signature_hex: &str, public_hex: &str) -> Result<(), PackError> {
    let pub_bytes = hex32(public_hex)?;
    let verifying = VerifyingKey::from_bytes(&pub_bytes)
        .map_err(|e| PackError::Other(format!("public key: {e}")))?;
    let sig_bytes = hex64(signature_hex)?;
    let sig = Signature::from_bytes(&sig_bytes);
    verifying
        .verify(bytes, &sig)
        .map_err(|_| PackError::Other("signature does not verify".into()))
}

/// Digest of an unpacked plugin tree: every file path and its content, in
/// sorted order — the lockfile pins this, so any later change is visible.
pub fn tree_digest(root: &Path) -> Result<String, PackError> {
    let mut hasher = Sha256::new();
    let mut paths: Vec<PathBuf> = walk(root)?;
    paths.sort();
    for path in paths {
        let rel = path.strip_prefix(root).unwrap_or(&path);
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        let bytes = std::fs::read(&path).map_err(|e| io(&path, e))?;
        hasher.update(&bytes);
        hasher.update(b"\0");
    }
    Ok(hex(&hasher.finalize()))
}

fn walk(dir: &Path) -> Result<Vec<PathBuf>, PackError> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| io(dir, e))? {
        let entry = entry.map_err(|e| io(dir, e))?;
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path)?);
        } else {
            out.push(path);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Lockfile
// ---------------------------------------------------------------------------

/// One installed plugin's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledPlugin {
    pub id: String,
    pub version: String,
    /// Digest of the unpacked tree at install time.
    pub digest: String,
    /// The verifying key (hex) that signed the install — the TOFU pin.
    pub signed_by: String,
    /// `enabled` is policy: only the CLI flips it.
    pub enabled: bool,
}

/// `<data>/plugins/installed.json`: what is installed, pinned and enabled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Lockfile {
    pub plugins: Vec<InstalledPlugin>,
}

impl Lockfile {
    pub fn load(path: &Path) -> Result<Self, PackError> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| PackError::Other(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(io(path, e)),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), PackError> {
        let text =
            serde_json::to_string_pretty(self).map_err(|e| PackError::Other(e.to_string()))?;
        std::fs::write(path, text).map_err(|e| io(path, e))
    }

    pub fn get(&self, id: &str) -> Option<&InstalledPlugin> {
        self.plugins.iter().find(|p| p.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut InstalledPlugin> {
        self.plugins.iter_mut().find(|p| p.id == id)
    }

    fn upsert(&mut self, plugin: InstalledPlugin) {
        match self.plugins.iter_mut().find(|p| p.id == plugin.id) {
            Some(slot) => *slot = plugin,
            None => self.plugins.push(plugin),
        }
    }
}

/// Verify, pin-check, unpack and record an archive. Returns the installed
/// record. `plugins_root` is `<data>/plugins`.
pub fn install_archive(
    archive: &SignedArchive,
    trust: &TrustStore,
    plugins_root: &Path,
) -> Result<InstalledPlugin, PackError> {
    if trust.keys.is_empty() {
        return Err(PackError::Other(
            "no trusted keys: run `ka-agent plugin keygen` first".into(),
        ));
    }
    // Signature must verify under exactly one trusted key.
    let mut verified_by: Option<String> = None;
    for public_hex in trust.keys.keys() {
        match verify_signature(&archive.bytes, &archive.signature, public_hex) {
            Ok(()) => {
                verified_by = Some(public_hex.clone());
                break;
            }
            Err(_) => continue,
        }
    }
    let Some(signed_by) = verified_by else {
        return Err(PackError::Other(
            "signature does not verify under any trusted key".into(),
        ));
    };
    let manifest = archive_manifest(&archive.bytes)?;
    manifest
        .validate()
        .map_err(|e| PackError::Other(format!("manifest: {e}")))?;
    let lock_path = plugins_root.join("installed.json");
    let mut lock = Lockfile::load(&lock_path)?;
    // TOFU: updates of an installed id must come from the pinned key.
    if let Some(existing) = lock.get(&manifest.plugin.id)
        && existing.signed_by != signed_by
    {
        return Err(PackError::Other(format!(
            "plugin {} is pinned to a different key; refusing this update",
            manifest.plugin.id
        )));
    }
    let target = plugins_root.join(&manifest.plugin.id);
    let staging = plugins_root.join(format!(".stage-{}", manifest.plugin.id));
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|e| io(&staging, e))?;
    }
    std::fs::create_dir_all(&staging).map_err(|e| io(&staging, e))?;
    let mut reader = std::io::Cursor::new(&archive.bytes);
    let mut tar = tar::Archive::new(&mut reader);
    tar.unpack(&staging)
        .map_err(|e| PackError::Other(format!("unpack: {e}")))?;
    // Validate what was actually unpacked, not just the copy in the archive.
    let unpacked_manifest = Manifest::load(&staging.join(MANIFEST_NAME))
        .map_err(|e| PackError::Other(format!("{e}")))?;
    if unpacked_manifest != manifest {
        return Err(PackError::Other(
            "unpacked manifest differs from the signed one".into(),
        ));
    }
    let digest = tree_digest(&staging)?;
    if target.exists() {
        std::fs::remove_dir_all(&target).map_err(|e| io(&target, e))?;
    }
    std::fs::rename(&staging, &target).map_err(|e| io(&target, e))?;
    // Installing never enables; it also never *disables*: an update keeps the
    // operator's enable state (only `plugin enable|disable` is policy).
    let enabled = lock.get(&manifest.plugin.id).is_some_and(|p| p.enabled);
    let record = InstalledPlugin {
        id: manifest.plugin.id.clone(),
        version: manifest.plugin.version.clone(),
        digest,
        signed_by,
        enabled,
    };
    lock.upsert(record.clone());
    lock.save(&lock_path)?;
    Ok(record)
}

/// Persist a keypair on disk (0600 seed, 0644 public half).
pub fn write_keypair(plugins_root: &Path, key: &SigningKey, label: &str) -> Result<(), PackError> {
    let dir = plugins_root.join("keys");
    std::fs::create_dir_all(&dir).map_err(|e| io(&dir, e))?;
    let name = key.public_hex()[..16].to_string();
    let seed_path = dir.join(format!("{name}.seed"));
    std::fs::write(&seed_path, key.seed_hex()).map_err(|e| io(&seed_path, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&seed_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| io(&seed_path, e))?;
    }
    let pub_path = dir.join(format!("{name}.pub"));
    let pub_line = format!("{}\t{}\n", key.public_hex(), label);
    std::fs::write(&pub_path, pub_line).map_err(|e| io(&pub_path, e))?;
    Ok(())
}

/// Load every trusted public key from `<plugins>/keys/*.pub`.
pub fn load_trust(plugins_root: &Path) -> Result<TrustStore, PackError> {
    let mut trust = TrustStore::default();
    let dir = plugins_root.join("keys");
    if !dir.exists() {
        return Ok(trust);
    }
    for entry in std::fs::read_dir(&dir).map_err(|e| io(&dir, e))? {
        let entry = entry.map_err(|e| io(&dir, e))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("pub") {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|e| io(&path, e))?;
        let Some((public_hex, label)) = text.trim().split_once('\t') else {
            continue;
        };
        trust
            .keys
            .insert(public_hex.trim().to_string(), label.trim().to_string());
    }
    Ok(trust)
}

/// Load the instance signing seed (first `*.seed`), if present.
pub fn load_signing_key(plugins_root: &Path) -> Result<Option<SigningKey>, PackError> {
    let dir = plugins_root.join("keys");
    if !dir.exists() {
        return Ok(None);
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| io(&dir, e))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("seed"))
        .collect();
    entries.sort();
    let Some(path) = entries.first() else {
        return Ok(None);
    };
    let seed = std::fs::read_to_string(path).map_err(|e| io(path, e))?;
    Ok(Some(SigningKey::from_seed_hex(seed.trim())?))
}

/// Write a detached signature file next to an archive.
pub fn write_signature(archive_path: &Path, signature: &str) -> Result<PathBuf, PackError> {
    let sig_path = archive_path.with_extension(format!("{ARCHIVE_EXT}.sig"));
    std::fs::write(&sig_path, signature).map_err(|e| io(&sig_path, e))?;
    Ok(sig_path)
}

/// Read an archive and its detached signature from disk.
pub fn read_archive(archive_path: &Path) -> Result<SignedArchive, PackError> {
    let bytes = std::fs::read(archive_path).map_err(|e| io(archive_path, e))?;
    let sig_path = archive_path.with_extension(format!("{ARCHIVE_EXT}.sig"));
    let signature = std::fs::read_to_string(&sig_path)
        .map_err(|e| io(&sig_path, e))?
        .trim()
        .to_string();
    Ok(SignedArchive { bytes, signature })
}

/// Write archive bytes to disk (pack → file).
pub fn write_archive(archive_path: &Path, bytes: &[u8]) -> Result<(), PackError> {
    std::fs::write(archive_path, bytes).map_err(|e| io(archive_path, e))?;
    let _ = std::io::stdout().flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn plugin_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ka-plug-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(
            dir.join(MANIFEST_NAME),
            r#"
[plugin]
id = "probe"
version = "0.1.0"
api = 1
description = "probe"
entry = { type = "mcp-stdio", command = "/plugin/bin/probe" }

[[tools]]
name = "probe.env"
effect = "read"
"#,
        )
        .unwrap();
        std::fs::write(dir.join("bin/probe"), "#!/bin/sh\n").unwrap();
        dir
    }

    fn trust_with(key: &SigningKey) -> TrustStore {
        let mut trust = TrustStore::default();
        trust.keys.insert(key.public_hex(), "test".to_string());
        trust
    }

    #[test]
    fn pack_install_and_tamper_detection() {
        let root = tempfile::tempdir().unwrap();
        let plugins = root.path().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let key = SigningKey::generate().unwrap();
        let trust = trust_with(&key);
        let dir = plugin_dir("ok");
        let archive = pack(&dir, &key).unwrap();

        let record = install_archive(&archive, &trust, &plugins).unwrap();
        assert!(!record.enabled, "install never enables");
        let installed = plugins.join("probe/bin/probe");
        assert!(installed.exists());

        // Tamper with the installed tree: the digest no longer matches.
        std::fs::write(&installed, "#!/bin/sh\necho pwned\n").unwrap();
        let now = tree_digest(&plugins.join("probe")).unwrap();
        assert_ne!(now, record.digest, "tampering must change the digest");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsigned_and_wrong_key_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let plugins = root.path().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let signer = SigningKey::generate().unwrap();
        let dir = plugin_dir("unsigned");
        let archive = pack(&dir, &signer).unwrap();

        // Empty trust: no keys at all.
        let err = install_archive(&archive, &TrustStore::default(), &plugins)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no trusted keys"), "{err}");

        // Signed by a key nobody trusts.
        let stranger = trust_with(&SigningKey::generate().unwrap());
        let err = install_archive(&archive, &stranger, &plugins)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not verify"), "{err}");

        // Tampered bytes under the right key.
        let trust = trust_with(&signer);
        let mut bytes = archive.bytes.clone();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        let tampered = SignedArchive {
            bytes,
            signature: archive.signature.clone(),
        };
        let err = install_archive(&tampered, &trust, &plugins)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not verify"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reinstall_keeps_the_operators_enable_state() {
        let root = tempfile::tempdir().unwrap();
        let plugins = root.path().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let key = SigningKey::generate().unwrap();
        let trust = trust_with(&key);
        let dir = plugin_dir("enable");
        let archive = pack(&dir, &key).unwrap();
        install_archive(&archive, &trust, &plugins).unwrap();

        let lock_path = plugins.join("installed.json");
        let mut lock = Lockfile::load(&lock_path).unwrap();
        lock.get_mut("probe").unwrap().enabled = true;
        lock.save(&lock_path).unwrap();

        // Reinstall: still enabled.
        install_archive(&archive, &trust, &plugins).unwrap();
        assert!(
            Lockfile::load(&lock_path)
                .unwrap()
                .get("probe")
                .unwrap()
                .enabled
        );

        // And an install of a different plugin stays disabled.
        let dir2 = plugin_dir("enable2");
        let other_manifest = std::fs::read_to_string(dir2.join(MANIFEST_NAME))
            .unwrap()
            .replace("id = \"probe\"", "id = \"other\"")
            .replace("\"probe.env\"", "\"other.env\"");
        std::fs::write(dir2.join(MANIFEST_NAME), other_manifest).expect("write other manifest");
        let other = pack(&dir2, &key).unwrap();
        install_archive(&other, &trust, &plugins).unwrap();
        let lock = Lockfile::load(&lock_path).unwrap();
        assert!(!lock.get("other").unwrap().enabled);
        assert!(lock.get("probe").unwrap().enabled);

        for d in [dir, dir2] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn updates_are_pinned_to_the_first_key() {
        let root = tempfile::tempdir().unwrap();
        let plugins = root.path().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let mine = SigningKey::generate().unwrap();
        let trust = trust_with(&mine);
        let dir = plugin_dir("pin1");
        install_archive(&pack(&dir, &mine).unwrap(), &trust, &plugins).unwrap();

        // An update signed by a different — even trusted — key: refused.
        let other = SigningKey::generate().unwrap();
        let mut both = trust.clone();
        both.keys.insert(other.public_hex(), "other".into());
        let dir2 = plugin_dir("pin2");
        let update = pack(&dir2, &other).unwrap();
        let err = install_archive(&update, &both, &plugins)
            .unwrap_err()
            .to_string();
        assert!(err.contains("pinned to a different key"), "{err}");

        // The pinned key updates fine.
        let dir3 = plugin_dir("pin3");
        install_archive(&pack(&dir3, &mine).unwrap(), &both, &plugins).unwrap();

        for d in [dir, dir2, dir3] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}
