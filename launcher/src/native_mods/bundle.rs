//! A native mod project as a build reads it: copied from the game's `sunrise-native-build` (tests/test_launcher_copies.py).
use crate::i18n::trf;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

mod package;

/// What a native build compiles: the game, or the headless engine a dedicated server runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Target {
    /// The game: `sunrise-client` as the release ships it.
    #[default]
    Client,
    /// The headless engine, built without the default features (`sunrise-engine`).
    Server,
}

impl Target {
    pub fn name(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Server => "server",
        }
    }
}

/// The horizon-mixins commit a mod that pins none builds with: the game's `mixins_tool::REV`.
pub const MIXINS_REV: &str = "e59da9094f0c2fdda6c627fe9eff22dec6a906f0";

/// A registry download's `sunrise-mod.toml` at the `.smod` package root.
pub const NATIVE_MANIFEST: &str = sunrise_launcher_smod::manifest::MANIFEST;

/// A validated flat mod project. The package manifest owns identity and build pins.
#[derive(Clone, Debug, PartialEq)]
pub struct Bundle {
    pub manifest: sunrise_launcher_smod::manifest::Manifest,
    pub dir: PathBuf,
}

impl Bundle {
    pub fn native(&self) -> &sunrise_launcher_smod::manifest::Native {
        self.manifest
            .native
            .as_ref()
            .expect("validated code package")
    }
}

/// The flat native project in `folder`, or the native projects directly inside it.
pub fn bundles(folder: &Path) -> Result<Vec<Bundle>, String> {
    let found = package::checked_bundles(folder)?;
    if found.is_empty() {
        return Err(trf(
            "{folder} holds no native mod ({manifest} and Cargo.toml).",
            &[
                ("folder", &folder.display().to_string()),
                ("manifest", NATIVE_MANIFEST),
            ],
        ));
    }
    Ok(found)
}

pub fn scan_bundles(folder: &Path) -> Result<Vec<Bundle>, String> {
    if !folder.exists() {
        return Ok(Vec::new());
    }
    if let Some(found) = bundle_in(folder)? {
        return Ok(vec![found]);
    }
    let mut dirs: Vec<PathBuf> = fs::read_dir(folder)
        .map_err(|e| {
            trf(
                "Could not read {path}: {error}",
                &[
                    ("path", &folder.display().to_string()),
                    ("error", &e.to_string()),
                ],
            )
        })?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    let mut found = Vec::new();
    for dir in dirs {
        found.extend(bundle_in(&dir)?);
    }
    Ok(found)
}

pub fn bundle_in(dir: &Path) -> Result<Option<Bundle>, String> {
    package::read(dir)
}

pub fn is_revision(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Bundle files omit only the API package's Cargo build output.
pub fn bundle_files(bundle: &Bundle) -> Result<Vec<(String, PathBuf)>, String> {
    package::files(bundle)
}

/// A digest of a mod's files (name, length, bytes, sorted by name), kept in `native-mods.json`.
pub fn digest<'a>(files: impl Iterator<Item = (&'a str, &'a [u8])>) -> String {
    let mut hash = Sha256::new();
    for (name, bytes) in files {
        hash.update(name.as_bytes());
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The selected flat files, in the same order as `native_status::enabled` hashes an archive.
pub fn source_digest(bundle: &Bundle) -> Option<String> {
    let paths = bundle_files(bundle);
    let read: Vec<_> = paths
        .ok()?
        .into_iter()
        .map(|(name, path)| Some((name, fs::read(path).ok()?)))
        .collect::<Option<_>>()?;
    Some(digest(read.iter().map(|(n, b)| (n.as_str(), b.as_slice()))))
}

/// Copy a bundle without its API package's Cargo build output.
pub fn copy_bundle(bundle: &Bundle, to: &Path) -> Result<(), String> {
    package::copy(bundle, to)
}
