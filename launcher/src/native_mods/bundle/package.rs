//! Validated flat packages: copied from the game's `sunrise-native-build` (tests/test_launcher_copies.py).
use super::Bundle;
use crate::i18n::{tr, trf};
use std::{fs, path::Path};
use sunrise_launcher_smod::manifest::USES_FILE;
use sunrise_launcher_smod::{manifest, Package};

pub fn read(dir: &Path) -> Result<Option<Bundle>, String> {
    for old in [
        "sunrise-native.json",
        "horizon-mixin.toml",
        "native",
        "rust",
    ] {
        if dir.join(old).exists() {
            return Err(trf(
                "{path}: unsupported package path {old}",
                &[("path", &dir.display().to_string()), ("old", old)],
            ));
        }
    }
    if !dir.join(manifest::MANIFEST).is_file() {
        return Ok(None);
    }
    let package = Package::read(dir)?;
    if package.manifest.native.is_none()
        && (package.manifest.wasm.is_none() || package.manifest.inject.is_empty())
    {
        return Ok(None);
    }
    if let Some(state) = package
        .manifest
        .native
        .as_ref()
        .and_then(|native| native.state.first())
    {
        return Err(trf(
            "E_MIXIN_STATE_LAYOUT: {mod}:{state}: native state changes the prebuilt game's layout; use an attachment instead",
            &[("mod", &package.manifest.id), ("state", &state.id)],
        ));
    }
    package.directory_files(dir)?;
    Ok(Some(Bundle {
        manifest: package.manifest,
        dir: dir.to_owned(),
    }))
}

fn checked_files(bundle: &Bundle) -> Result<std::collections::BTreeMap<String, Vec<u8>>, String> {
    Package::read(&bundle.dir)?.directory_files(&bundle.dir)
}

pub fn files(bundle: &Bundle) -> Result<Vec<(String, std::path::PathBuf)>, String> {
    Ok(checked_files(bundle)?
        .into_keys()
        .map(|name| {
            let path = bundle.dir.join(&name);
            (name, path)
        })
        .collect())
}

/// The package's files and its uses table, which is judged against another build as the package was.
pub fn copy(bundle: &Bundle, to: &Path) -> Result<(), String> {
    let mut files = checked_files(bundle)?;
    if let Ok(table) = fs::read(bundle.dir.join(USES_FILE)) {
        files.insert(USES_FILE.to_owned(), table);
    }
    copy_files(to, files)
}

fn copy_files(to: &Path, files: impl IntoIterator<Item = (String, Vec<u8>)>) -> Result<(), String> {
    for (name, bytes) in files {
        let path = to.join(name);
        fs::create_dir_all(path.parent().ok_or(tr("invalid destination"))?)
            .map_err(|e| e.to_string())?;
        fs::write(path, bytes).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Check target-crossing edges; an unreadable package stops the scan, naming the mod, rather than building a smaller set.
pub fn checked_bundles(folder: &Path) -> Result<Vec<Bundle>, String> {
    let paths = sunrise_launcher_smod::find(folder)
        .into_iter()
        .chain(sunrise_launcher_smod::find(
            &folder
                .parent()
                .unwrap_or(folder)
                .join(sunrise_launcher_smod::mod_list::DIR),
        ));
    if let Some((id, error)) = sunrise_launcher_smod::selection::package_errors(paths)
        .into_iter()
        .next()
    {
        return Err(format!("{id}: {error}"));
    }
    let found = super::scan_bundles(folder)?;
    let wasm: Vec<_> = sunrise_launcher_smod::find(folder)
        .into_iter()
        .chain(sunrise_launcher_smod::find(
            &folder
                .parent()
                .unwrap_or(folder)
                .join(sunrise_launcher_smod::mod_list::DIR),
        ))
        .filter_map(|path| sunrise_launcher_smod::Package::read(&path).ok())
        .filter(|package| package.manifest.wasm.is_some())
        .collect();
    for bundle in &found {
        let dependent = sunrise_launcher_smod::Package::read(&bundle.dir)?;
        for library in &wasm {
            if !dependent
                .manifest
                .dependencies
                .contains_key(&library.manifest.id)
            {
                continue;
            }
            let reason = if let Some(name) = dependent
                .layouts
                .first_difference(&library.manifest.id, &library.layouts)
            {
                trf(
                    "{mod}: layout mismatch in library {library} for type {type}",
                    &[
                        ("mod", &dependent.manifest.id),
                        ("library", &library.manifest.id),
                        ("type", name),
                    ],
                )
            } else {
                trf(
                    "{library} is a wasm mod; native mod {mod} depends on it: mixed native and wasm dependencies aren't supported yet",
                    &[
                        ("library", &library.manifest.id),
                        ("mod", &dependent.manifest.id),
                    ],
                )
            };
            return Err(reason);
        }
    }
    Ok(found)
}
