//! The Ka Agent plugin host (SEED.md §6, Phase 5): manifest v1, signed
//! archives with TOFU pinning, and a bubblewrap sandbox runner. Third-party
//! code runs as an MCP-stdio subprocess behind the bridge; installing a
//! plugin never grants anything — scopes are separate operator grants.

pub mod archive;
pub mod manifest;
pub mod sandbox;

pub use archive::{
    ARCHIVE_EXT, InstalledPlugin, Lockfile, MANIFEST_NAME, PackError, SignedArchive, SigningKey,
    TrustStore, archive_manifest, install_archive, load_signing_key, load_trust, pack,
    read_archive, tree_digest, write_archive, write_keypair, write_signature,
};
pub use manifest::{Manifest, ManifestError, PLUGIN_API};
pub use sandbox::{CALL_TIMEOUT, OUTPUT_CAP, Sandbox, SandboxError};
