//! A mod author's discovery and registry lock through the mod's SDK tool (`native-discover`, `native-lock`), with the launcher's managed Rust and native cache.
use super::{root, rust_toolchain, tool};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Runs the tool for the mod in `folder` on `command` with `plan` plus the work folder, the toolchain and the mod.
fn ask(folder: &Path, command: &str, mut plan: Value) -> Result<Value, String> {
    let rust = rust_toolchain::install(|_| {})?;
    let program = super::tool_for(folder)?;
    plan["work"] = json!(root());
    plan["toolchain"] = tool::toolchain(&rust);
    plan["mods"] = json!(folder);
    tool::ask(rust.command(&program), command, &plan)
}

/// The internals the mod in `folder` names, as the pinned game's catalog lists them (the tool's JSON).
pub(crate) fn check(folder: &Path) -> Result<Value, String> {
    Ok(ask(folder, "native-discover", json!({}))?["internals"].take())
}

/// The pinned game's build data with only this mod's modules applied, copied into `destination` while no native build can replace it.
pub fn catalog(folder: &Path, destination: &Path) -> Result<PathBuf, String> {
    super::require_consent().map_err(|e| e.to_string())?;
    let _lease = crate::native_cache::Cache::acquire(&crate::backend::build_dir())
        .map_err(super::cache_error)?;
    super::clear_log()?;
    let reply = ask(folder, "native-discover", json!({"catalog": destination}))?;
    Ok(reply["catalog"]
        .as_str()
        .map_or_else(|| destination.to_path_buf(), PathBuf::from))
}

/// Pins the mod's registry dependencies in its `Cargo.lock`; never compiles source or changes the manifest.
pub(crate) fn lock(folder: &Path, update: bool) -> Result<(), String> {
    ask(folder, "native-lock", json!({"update": update})).map(drop)
}
