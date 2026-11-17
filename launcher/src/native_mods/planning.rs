//! Read-only classification of a native mod set, without executing mod or game code.
use super::*;

/// Plan enabled native mods for an instance. Worker-only filesystem and release reads.
pub fn routes(home: &Path, base: Option<&str>) -> Result<Routes, Failed> {
    require_consent()?;
    let lease = crate::native_cache::Cache::acquire(&backend::build_dir()).map_err(cache_error)?;
    let Some(folder) = stage_installed(home, root().join("installed.routes"))? else {
        return Ok(Routes::default());
    };
    classify(&folder, base, &lease)
}

/// Share classification with an actual build under its existing cache lease.
pub(super) fn classify(
    folder: &Path,
    base: Option<&str>,
    // Held while the SDK's index, surface and build data are read and kept.
    _lease: &crate::native_cache::Cache,
) -> Result<Routes, Failed> {
    let found: Vec<Bundle> = scan_bundles(folder)?
        .into_iter()
        .filter(|bundle| bundle.manifest.native.is_some())
        .collect();
    let mut result = Routes::default();
    for bundle in &found {
        result
            .mods
            .push((bundle.manifest.id.clone(), Route::Prebuilt));
    }
    if found.is_empty() {
        return Ok(result);
    }
    let ids: Vec<&str> = found.iter().map(|b| b.manifest.id.as_str()).collect();
    let base = base.ok_or_else(|| tr("Install Sunrise first.").to_string())?;
    let built_on = checked_about(base)?;
    let origin = built_on.as_ref().map_or(base, |about| about.base.as_str());
    let revision = build_revision(base, built_on.as_ref())?;
    // A developer install's checkout has no published surface: its mods are made for it exactly.
    if crate::developer_install::is_developer(origin) {
        if let Some(list) = mismatch(&found, &revision) {
            return Err(mismatch_error(&list).into());
        }
        return Ok(result);
    }
    let sdk_base = sdk_cache::Base::from_env();
    let keys = sunrise_launcher_release_verify::sdk_keys_for(backend::REPO);
    let sdk = match sdk_cache::index(&sdk_base, &revision, &keys) {
        Ok(sdk) => sdk,
        Err(refusal) => {
            result.unavailable = Some(refusal);
            return Ok(result);
        }
    };
    let shown = sdk.shown();
    let mut said = |id: &str, route: Route| {
        let (_, slot) = result
            .mods
            .iter_mut()
            .find(|(mod_id, _)| mod_id == id)
            .expect("parsed project has a route");
        if matches!(slot, Route::Prebuilt) {
            *slot = route;
        }
    };
    match check_all(folder, &ids, &sdk, &sdk_base) {
        Ok(checks) => {
            for (id, reason) in checks.refusals {
                said(id.as_str(), Route::Invalid { reason });
            }
            for (id, verdict) in checks.verdicts {
                let route = match &verdict {
                    fit::Verdict::MadeFor => continue,
                    fit::Verdict::Fits { .. } => Route::Fits {
                        line: fit::line(&id, &verdict, &shown),
                    },
                    _ => Route::Unfit {
                        headline: fit::headline(&id, &verdict),
                        line: fit::line(&id, &verdict, &shown),
                        actions: fit::actions(&id),
                    },
                };
                said(id.as_str(), route);
            }
        }
        // Nothing is guessed: without the build's files, no mod made for another build "fits".
        Err(refusal) => {
            for bundle in &found {
                let id = &bundle.manifest.id;
                if bundle.native().sunrise != revision {
                    said(
                        id.as_str(),
                        Route::Invalid {
                            reason: trf(
                                "Can't check {mod} against {build}. {why}",
                                &[
                                    ("mod", id),
                                    ("build", &shown),
                                    ("why", &refusal.to_string()),
                                ],
                            ),
                        },
                    );
                }
            }
        }
    }
    Ok(result)
}

/// What updating an instance's native mods to a newer release or Nightly would do, told before
/// anything is downloaded but the build's small SDK files (design-fit §10.1).
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateCheck {
    /// The newer installed release or Nightly the update builds on.
    pub to: String,
    /// What the player reads: how many mods fit, and each one switched off and why.
    pub summary: String,
    /// The worlds saved with a mod the update would switch off: then backing them up is the
    /// other choice to staying.
    pub worlds: Vec<String>,
}

/// [`UpdateCheck`] for the native mods turned on in game folder `home`, on the installed release
/// or Nightly `to`. Worker-only: it reads releases.
pub fn update_check(home: &Path, to: &str) -> Result<UpdateCheck, Failed> {
    require_consent()?;
    let _lease = crate::native_cache::Cache::acquire(&backend::build_dir()).map_err(cache_error)?;
    let folder = stage_installed(home, root().join("installed.routes"))?;
    let found = match &folder {
        Some(folder) => scan_bundles(folder)?,
        None => Vec::new(),
    };
    let found: Vec<Bundle> = found
        .into_iter()
        .filter(|bundle| bundle.manifest.native.is_some())
        .collect();
    let ids: Vec<&str> = found.iter().map(|b| b.manifest.id.as_str()).collect();
    let revision = build_revision(to, None)?;
    let sdk_base = sdk_cache::Base::from_env();
    let keys = sunrise_launcher_release_verify::sdk_keys_for(backend::REPO);
    let sdk = sdk_cache::index(&sdk_base, &revision, &keys).map_err(|r| r.to_string())?;
    let verdicts = match &folder {
        Some(folder) => {
            check_all(folder, &ids, &sdk, &sdk_base)
                .map_err(|r| r.to_string())?
                .verdicts
        }
        None => Vec::new(),
    };
    let shown = sdk.shown();
    let unfit: Vec<LeftOut> = verdicts
        .iter()
        .filter(|(_, verdict)| !verdict.usable())
        .filter_map(|(id, verdict)| {
            let bundle = found.iter().find(|b| &b.manifest.id == id)?;
            Some(LeftOut::new(
                bundle,
                isolate::Why::Unfit(fit::short(verdict)),
            ))
        })
        .collect();
    let declared: Vec<Vec<dependencies::Dependency>> = found
        .iter()
        .map(|bundle| {
            dependencies::from_manifest(
                &bundle.manifest.dependencies,
                sunrise_mod_package::manifest::Side::Both,
            )
        })
        .collect();
    let pairs: Vec<(&Bundle, &[dependencies::Dependency])> = found
        .iter()
        .zip(&declared)
        .map(|(b, d)| (b, d.as_slice()))
        .collect();
    let left = isolate::with_dependents(&unfit, &pairs);
    let off: Vec<(String, String)> = left
        .iter()
        .map(|l| {
            let why = match &l.why {
                isolate::Why::Unfit(why) => why.clone(),
                isolate::Why::Needs(library) => trf(
                    "it needs {library}, which doesn't fit",
                    &[("library", library)],
                ),
                _ => String::new(),
            };
            (l.id.clone(), why)
        })
        .collect();
    let ids: Vec<String> = left.iter().map(|l| l.id.clone()).collect();
    let all = crate::worlds::list(home);
    let worlds = crate::worlds::saved_with(&all, &ids)
        .into_iter()
        .map(|(world, _)| world.name.clone())
        .collect();
    Ok(UpdateCheck {
        to: to.to_owned(),
        summary: fit::update_summary(&shown, found.len(), &off),
        worlds,
    })
}
