//! Locating, loading and integrity-checking the vendored spec.
//!
//! Three files live in `spec/` and each has a distinct job:
//!
//! | file | job |
//! | --- | --- |
//! | `v1_json.tmpl` | byte-for-byte upstream vendor. Provenance only — **never parsed** |
//! | `gitea-v<ver>.json` | de-templated, key-sorted canonical JSON. The generator's input |
//! | `lock.toml` | the tag, the source URL, and a sha256 of each of the two files above |
//!
//! [`load`] verifies the canonical file against the sha256 in `lock.toml` on every run. That
//! is not paranoia about corruption: it is what makes a hand-edit of the vendored spec
//! impossible to hide. The generated file headers record that same sha256, so "which spec did
//! this code come from" is answerable from the tree alone.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Result;
use crate::swagger::Spec;

/// Where the spec lives upstream. Not a release asset — it is a template in the source tree,
/// which is why `update-spec` fetches from `/raw/tag/<tag>/` rather than an API.
pub fn source_url(tag: &str) -> String {
    format!(
        "https://raw.githubusercontent.com/go-gitea/gitea/refs/tags/{tag}/templates/swagger/v1_json.tmpl"
    )
}

/// The same template on a moving branch — `main` is upstream's development branch. Only
/// `spec-diff` reads this; `update-spec` deliberately cannot, because a branch is not a
/// version and the vendored spec must stay pinnable.
pub fn branch_url(branch: &str) -> String {
    format!(
        "https://raw.githubusercontent.com/go-gitea/gitea/refs/heads/{branch}/templates/swagger/v1_json.tmpl"
    )
}

pub fn spec_dir(root: &Path) -> PathBuf {
    root.join("spec")
}

pub fn tmpl_path(root: &Path) -> PathBuf {
    spec_dir(root).join("v1_json.tmpl")
}

pub fn lock_path(root: &Path) -> PathBuf {
    spec_dir(root).join("lock.toml")
}

pub fn canonical_name(version: &str) -> String {
    format!("gitea-v{version}.json")
}

pub fn name_lock_path(root: &Path) -> PathBuf {
    spec_dir(root).join("name-lock.toml")
}

/// `spec/lock.toml`.
///
/// Both hashes are recorded because they answer different questions. `upstream_sha256` proves
/// *what we fetched* (and lets a future audit re-fetch the tag and compare). `canonical_sha256`
/// proves *what the generator read*, and is the value stamped into every generated file's
/// header.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lock {
    /// The git tag, e.g. `v1.27.2`.
    pub tag: String,
    /// The tag without its `v`, e.g. `1.27.2`. This is what `info.version` is set to and what
    /// `gea --version` reports.
    pub version: String,
    pub source: String,
    pub upstream: String,
    pub upstream_sha256: String,
    pub canonical: String,
    pub canonical_sha256: String,
}

pub struct Loaded {
    pub lock: Lock,
    pub spec: Spec,
    /// The canonical JSON text, kept so callers can re-hash or re-serialize without a second
    /// read.
    pub json: String,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub fn load_lock(root: &Path) -> Result<Lock> {
    let path = lock_path(root);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "cannot read {}: {e}\n  run: cargo xtask update-spec --version v1.27.2",
            path.display()
        )
    })?;
    Ok(toml::from_str(&text)?)
}

/// Reads and parses the canonical spec, refusing to proceed if it does not hash to the value
/// recorded in `lock.toml`.
pub fn load(root: &Path) -> Result<Loaded> {
    let lock = load_lock(root)?;
    let path = spec_dir(root).join(&lock.canonical);
    let json = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;

    let actual = sha256_hex(json.as_bytes());
    if actual != lock.canonical_sha256 {
        bail!(
            "{} does not match spec/lock.toml\n  \
             expected sha256: {}\n  \
             actual sha256:   {}\n  \
             The vendored spec is generated; edit it and every generated file's provenance \
             header becomes a lie. Re-run: cargo xtask update-spec --version {}",
            path.display(),
            lock.canonical_sha256,
            actual,
            lock.tag,
        );
    }

    let spec: Spec = serde_json::from_str(&json)
        .map_err(|e| format!("{} is not a spec we understand: {e}", path.display()))?;

    if spec.swagger != "2.0" {
        bail!(
            "spec declares swagger {:?}; this generator models Swagger 2.0 only. \
             An OpenAPI 3 spec needs a new lowering pass, not a tweak.",
            spec.swagger
        );
    }

    Ok(Loaded { lock, spec, json })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_reference_implementation() {
        // Guards against a hand-rolled hex encoder dropping a leading zero, which would make
        // lock.toml disagree with `sha256sum` and send someone on a long hunt.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn source_url_points_at_the_tag_not_a_branch() {
        // Fetching from a branch would make the vendored spec unpinnable.
        assert_eq!(
            source_url("v1.27.2"),
            "https://raw.githubusercontent.com/go-gitea/gitea/refs/tags/v1.27.2/templates/swagger/v1_json.tmpl"
        );
    }
}
