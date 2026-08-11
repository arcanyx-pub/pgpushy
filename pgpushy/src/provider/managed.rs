//! The managed backend: download a pinned pgschema and cache it (spec §8.5).
//!
//! The default, because "install pgschema first" is a step pgpushy can simply
//! remove. The cost is that pgpushy now fetches an executable over the
//! network, so the integrity story has to be real rather than decorative:
//!
//! - **pgschema publishes no checksums**, so pgpushy ships its own, computed
//!   from the release assets and reviewed alongside the version bump that adds
//!   them ([`HASHES`]). TLS alone would mean trusting whatever the endpoint
//!   served today.
//! - A version pgpushy has **no** shipped hash for is still usable — pinning a
//!   newer pgschema should not require a pgpushy release — but pgpushy says
//!   plainly that it is trusting TLS only (spec §8.5).
//! - The cache is written **atomically**, so a killed download cannot leave a
//!   truncated binary that later runs would happily execute.
//! - The cache is **re-verified on every hit**, not trusted because the file
//!   exists. Atomicity only covers pgpushy's own writes; it says nothing about
//!   what else may have touched the cache since. Hashing ~19 MB costs about
//!   10 ms, which is nothing beside the network and database work that
//!   follows.

use super::{MIN_PGSCHEMA, PgschemaBin, PgschemaProvider};
use anyhow::{Context, Result, bail};
use semver::Version;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

/// The pgschema version pgpushy pins by default.
///
/// The same version §13 makes the floor, and for the same reason: it is what
/// CI tests against. Bumping this, `MIN_PGSCHEMA`, and the CI matrix is one
/// action, not three.
pub const PINNED_PGSCHEMA: &str = MIN_PGSCHEMA;

/// SHA-256 of each release asset pgpushy pins, as `(version, platform, hash)`.
///
/// Computed from the published assets and committed deliberately. pgschema
/// ships no checksums of its own, so this table is the only thing standing
/// between a compromised or swapped release asset and an executed binary.
///
/// Adding a version means adding four rows — one per platform — verified by
/// downloading each asset and hashing it, not by copying from anywhere.
const HASHES: &[(&str, &str, &str)] = &[
    (
        "1.12.0",
        "linux-amd64",
        "12610adf748b0dafe4e488ee7e9e68e6ffbef1f4e0f038dda36cf0138eede598",
    ),
    (
        "1.12.0",
        "linux-arm64",
        "58ec57023954a0239cf9d607c4e5432da6dd0b279399d1c318204120619a221d",
    ),
    (
        "1.12.0",
        "darwin-amd64",
        "c64b2ac24c4246344908910e892c4123be282bbb449f0b535079ff41d0f47c8f",
    ),
    (
        "1.12.0",
        "darwin-arm64",
        "f01ea488f21700752d5747bc013c406daa583a68b631739f33af430d5d3ec449",
    ),
];

/// Downloads and caches a pinned pgschema.
pub struct Managed {
    /// The version to fetch. Defaults to [`PINNED_PGSCHEMA`].
    pub version: String,
    /// Where cached binaries live. Defaults to the user's cache directory.
    pub cache_root: Option<PathBuf>,
}

impl PgschemaProvider for Managed {
    fn resolve(&self) -> Result<PgschemaBin> {
        let version = Version::parse(&self.version).with_context(|| {
            format!(
                "pinned pgschema version {:?} is not a version number",
                self.version
            )
        })?;
        // The floor applies here too. It is about what pgpushy is tested
        // against, not about how the binary arrived.
        let minimum = Version::parse(MIN_PGSCHEMA).expect("MIN_PGSCHEMA is valid semver");
        if version < minimum {
            bail!(
                "pinned pgschema {version} is below the minimum {minimum}\n\
                 \n\
                 Raise [pgschema] version, or remove it to use the version pgpushy \
                 pins by default ({PINNED_PGSCHEMA})."
            );
        }

        let platform = Platform::current()?;
        let cached = self.cache_path(&self.version, platform);
        let expected = expected_hash(&self.version, platform);

        if cached.exists() {
            match cache_is_intact(&cached, expected) {
                Ok(true) => {
                    return Ok(PgschemaBin {
                        path: cached,
                        version: Some(version),
                    });
                }
                Ok(false) => {
                    // Loud rather than silent: a cached binary that stopped
                    // matching is worth knowing about even though re-fetching
                    // fixes it, because corruption and tampering look the same
                    // from here.
                    crate::report::cache_mismatch(&cached);
                }
                // Unreadable is not a reason to fail; it is a reason to fetch
                // a fresh copy over the top.
                Err(_) => {}
            }
        }

        crate::report::downloading_pgschema(&self.version, platform.as_str(), expected.is_none());

        let bytes = download(&asset_url(&self.version, platform))?;
        verify(&bytes, expected, &self.version, platform)?;
        install(&cached, &bytes)?;

        crate::report::downloaded_pgschema(&cached);
        Ok(PgschemaBin {
            path: cached,
            version: Some(version),
        })
    }
}

impl Managed {
    /// Where a given version lives, per spec §8.5.
    fn cache_path(&self, version: &str, platform: Platform) -> PathBuf {
        let root = self.cache_root.clone().unwrap_or_else(default_cache_root);
        // Keyed by platform as well as version so that a cache shared over a
        // network home directory cannot serve one architecture's binary to
        // another.
        root.join("pgschema")
            .join(version)
            .join(platform.as_str())
            .join("pgschema")
    }
}

/// `$XDG_CACHE_HOME/pgpushy`, or the conventional fallback.
fn default_cache_root() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(xdg).join("pgpushy");
    }
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        // macOS convention differs, but pgschema publishes no Windows binary,
        // so these two are the whole world for this backend.
        let home = PathBuf::from(home);
        return if cfg!(target_os = "macos") {
            home.join("Library/Caches/pgpushy")
        } else {
            home.join(".cache/pgpushy")
        };
    }
    PathBuf::from(".pgpushy-cache")
}

/// The platforms pgschema publishes binaries for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Platform {
    LinuxAmd64,
    LinuxArm64,
    DarwinAmd64,
    DarwinArm64,
}

impl Platform {
    fn current() -> Result<Self> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => Ok(Self::LinuxAmd64),
            ("linux", "aarch64") => Ok(Self::LinuxArm64),
            ("macos", "x86_64") => Ok(Self::DarwinAmd64),
            ("macos", "aarch64") => Ok(Self::DarwinArm64),
            (os, arch) => bail!(
                "pgschema publishes no binary for {os}/{arch}\n\
                 \n\
                 The managed backend serves Linux and macOS on amd64 and arm64. \
                 Elsewhere — Windows especially — supply your own binary:\n\
                 \n    \
                 [pgschema]\n    \
                 backend = \"byo\"\n    \
                 path    = \"/path/to/pgschema\"",
            ),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::LinuxAmd64 => "linux-amd64",
            Self::LinuxArm64 => "linux-arm64",
            Self::DarwinAmd64 => "darwin-amd64",
            Self::DarwinArm64 => "darwin-arm64",
        }
    }
}

fn asset_url(version: &str, platform: Platform) -> String {
    format!(
        "https://github.com/pgplex/pgschema/releases/download/v{version}/pgschema-{version}-{}",
        platform.as_str(),
    )
}

fn expected_hash(version: &str, platform: Platform) -> Option<&'static str> {
    HASHES
        .iter()
        .find(|(v, p, _)| *v == version && *p == platform.as_str())
        .map(|(_, _, hash)| *hash)
}

/// Fetch over HTTPS (spec §8.5).
fn download(url: &str) -> Result<Vec<u8>> {
    let response = ureq::get(url)
        .call()
        .with_context(|| format!("downloading {url}"))?;

    let mut bytes = Vec::new();
    response
        .into_body()
        .into_reader()
        // ~18 MB assets; the cap is a backstop against a hostile response
        // streaming forever, not a real size limit.
        .take(256 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {url}"))?;

    if bytes.is_empty() {
        bail!("{url} returned an empty response");
    }
    Ok(bytes)
}

fn verify(bytes: &[u8], expected: Option<&str>, version: &str, platform: Platform) -> Result<()> {
    let Some(expected) = expected else {
        // Spec §8.5 permits this and requires saying so, which
        // `downloading_pgschema` already did.
        return Ok(());
    };

    let actual = hex(&Sha256::digest(bytes));
    if actual != expected {
        bail!(
            "checksum mismatch for pgschema {version} ({})\n\
             \n\
             expected {expected}\n\
             got      {actual}\n\
             \n\
             The download did not match the hash pgpushy ships for this version. \
             Nothing has been written to the cache. Do not work around this by \
             pinning a different version — investigate, or supply a binary you \
             trust with [pgschema] backend = \"byo\".",
            platform.as_str(),
        );
    }
    Ok(())
}

/// Write the binary into the cache, atomically and executable.
///
/// Atomic because the alternative is a truncated file that a later run finds
/// present, trusts, and executes: `exists()` is the cache check, so a partial
/// write would be indistinguishable from a good one.
fn install(destination: &Path, bytes: &[u8]) -> Result<()> {
    let directory = destination.parent().expect("cache path has a parent");
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating {}", directory.display()))?;

    // In the same directory, so the rename cannot cross a filesystem boundary.
    let mut temporary = tempfile::Builder::new()
        .prefix(".pgschema-")
        .tempfile_in(directory)
        .with_context(|| format!("creating a temporary file in {}", directory.display()))?;

    use std::io::Write;
    temporary
        .write_all(bytes)
        .context("writing the downloaded binary")?;
    temporary
        .flush()
        .context("flushing the downloaded binary")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o755))
            .context("making the downloaded binary executable")?;
    }

    // `persist` is a rename, so a concurrent pgpushy either sees no file or
    // sees the whole one — never a half-written binary.
    temporary
        .persist(destination)
        .map_err(|err| err.error)
        .with_context(|| format!("installing {}", destination.display()))?;
    Ok(())
}

/// Whether a cached binary still matches the hash pgpushy ships.
///
/// `true` when there is nothing to check against — an operator-pinned version
/// pgpushy has no hash for was already downloaded on TLS trust alone, and
/// re-fetching it every run would not improve on that.
fn cache_is_intact(path: &Path, expected: Option<&str>) -> Result<bool> {
    let Some(expected) = expected else {
        return Ok(true);
    };
    let bytes = std::fs::read(path)?;
    Ok(hex(&Sha256::digest(&bytes)) == expected)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_urls_match_pgschemas_release_naming() {
        assert_eq!(
            asset_url("1.12.0", Platform::LinuxAmd64),
            "https://github.com/pgplex/pgschema/releases/download/v1.12.0/pgschema-1.12.0-linux-amd64",
        );
        assert_eq!(
            asset_url("1.12.0", Platform::DarwinArm64),
            "https://github.com/pgplex/pgschema/releases/download/v1.12.0/pgschema-1.12.0-darwin-arm64",
        );
    }

    /// Every platform pgschema publishes must have a hash for the pinned
    /// version, or the default install path silently drops to TLS-only trust
    /// on whichever platform was forgotten.
    #[test]
    fn the_pinned_version_has_a_hash_for_every_platform() {
        for platform in [
            Platform::LinuxAmd64,
            Platform::LinuxArm64,
            Platform::DarwinAmd64,
            Platform::DarwinArm64,
        ] {
            assert!(
                expected_hash(PINNED_PGSCHEMA, platform).is_some(),
                "no shipped hash for {} on {}",
                PINNED_PGSCHEMA,
                platform.as_str(),
            );
        }
    }

    #[test]
    fn shipped_hashes_are_well_formed() {
        for (version, platform, hash) in HASHES {
            assert_eq!(hash.len(), 64, "{version} {platform}: not a sha256");
            assert!(
                hash.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{version} {platform}: not lowercase hex",
            );
        }
    }

    #[test]
    fn an_unknown_version_has_no_hash() {
        assert!(expected_hash("99.0.0", Platform::LinuxAmd64).is_none());
    }

    #[test]
    fn verify_accepts_the_matching_hash_and_rejects_others() {
        let bytes = b"pretend this is a binary";
        let correct = hex(&Sha256::digest(bytes));

        assert!(verify(bytes, Some(&correct), "1.0.0", Platform::LinuxAmd64).is_ok());

        let err = verify(bytes, Some(&"0".repeat(64)), "1.0.0", Platform::LinuxAmd64)
            .unwrap_err()
            .to_string();
        assert!(err.contains("checksum mismatch"), "{err}");
        // The message must not suggest pinning around it.
        assert!(err.contains("investigate"), "{err}");
    }

    /// Spec §8.5: a version pgpushy has no hash for is still usable, so that
    /// pinning a newer pgschema does not require a pgpushy release.
    #[test]
    fn verify_allows_a_version_with_no_shipped_hash() {
        assert!(verify(b"anything", None, "99.0.0", Platform::LinuxAmd64).is_ok());
    }

    #[test]
    fn the_cache_path_separates_versions_and_platforms() {
        let managed = Managed {
            version: "1.12.0".into(),
            cache_root: Some(PathBuf::from("/cache")),
        };
        assert_eq!(
            managed.cache_path("1.12.0", Platform::LinuxAmd64),
            PathBuf::from("/cache/pgschema/1.12.0/linux-amd64/pgschema"),
        );
        assert_ne!(
            managed.cache_path("1.12.0", Platform::LinuxAmd64),
            managed.cache_path("1.12.0", Platform::DarwinArm64),
        );
    }

    #[test]
    fn a_tampered_cache_is_detected() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("pgschema");
        std::fs::write(&path, b"the real binary").expect("write");
        let good = hex(&Sha256::digest(b"the real binary"));

        assert!(cache_is_intact(&path, Some(&good)).expect("readable"));

        std::fs::write(&path, b"something else entirely").expect("tamper");
        assert!(!cache_is_intact(&path, Some(&good)).expect("readable"));
    }

    /// A version with no shipped hash was fetched on TLS trust alone, and
    /// re-fetching every run would not improve on that.
    #[test]
    fn a_cache_with_no_known_hash_is_left_alone() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("pgschema");
        std::fs::write(&path, b"whatever").expect("write");
        assert!(cache_is_intact(&path, None).expect("readable"));
    }

    #[test]
    fn install_writes_an_executable_file() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let destination = dir.path().join("nested/pgschema");

        install(&destination, b"binary").expect("installs");

        assert_eq!(std::fs::read(&destination).expect("read"), b"binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&destination)
                .expect("stat")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o111,
                0o111,
                "should be executable by everyone who can read it"
            );
        }
    }
}
