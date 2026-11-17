//! How native mods stand against one game build before anything compiles: each mod's refusal and fit (SDK design §10), and the route the usable ones take.
use crate::{api, internals, prebuilt, scan_bundles};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use sunrise_mod_package::mod_surface::{
    self,
    uses::{self, Fit, Misfit, UsesTable},
};

/// How a mod stands against the build an instance plays.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Verdict {
    /// Made for this build.
    MadeFor,
    /// Made for another build (`made_for`), and every item it uses is unchanged in this one.
    Fits { made_for: String },
    /// It changes game code or names the game's internals (`why`: the table that does, or `internals`): it works on the build it was made for only.
    Exact { made_for: String, why: &'static str },
    /// Its package doesn't say what it uses (an older tool, or a list for another build than its pin): it works on the build it was made for only.
    NoList { made_for: String },
    /// Items it uses changed in this build.
    DoesNotFit {
        made_for: String,
        misfits: Vec<Misfit>,
    },
    /// An `[[inject]]` row's point is not the same in this build: why.
    Points { made_for: String, why: String },
}

impl Verdict {
    /// Whether the mod can be built against this build.
    pub fn usable(&self) -> bool {
        matches!(self, Self::MadeFor | Self::Fits { .. })
    }
}

/// The uses table `dir` carries, when it carries one this build reads; any other is no table, so the mod works on its own build only.
pub fn table(dir: &Path) -> Option<UsesTable> {
    let file = std::fs::File::open(dir.join(uses::USES_FILE)).ok()?;
    let mut bytes = Vec::new();
    file.take(uses::MAX_USES_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    UsesTable::read(&bytes).ok()
}

/// How `project` stands against build `build`, whose surface `surface` gives (asked for only when the mod was made for another build) and whose build data `catalog` holds.
pub fn judge<E>(
    project: &api::Project<'_>,
    build: &str,
    surface: &mut dyn FnMut() -> Result<Rc<mod_surface::Surface>, E>,
    catalog: &mut dyn FnMut() -> Result<Rc<internals::Catalog>, E>,
) -> Result<Verdict, E> {
    let made_for = project.bundle.native().sunrise.clone();
    if made_for == build {
        return Ok(Verdict::MadeFor);
    }
    if let Some(why) = prebuilt::changes_game(project) {
        return Ok(Verdict::Exact { made_for, why });
    }
    let Some(table) = table(&project.bundle.dir).filter(|table| table.made_for == made_for) else {
        return Ok(Verdict::NoList { made_for });
    };
    let target = surface()?;
    if let Fit::DoesNotFit(misfits) = uses::fits(&table.entries, target.as_ref()) {
        return Ok(Verdict::DoesNotFit { made_for, misfits });
    }
    if !project.bundle.manifest.inject.is_empty()
        && let Err(error) = catalog()?.check(&project.bundle.manifest)
    {
        return Ok(Verdict::Points {
            made_for,
            why: error.to_string(),
        });
    }
    Ok(Verdict::Fits { made_for })
}

/// What `cargo sunrise-mod native-check` checks: the first line of standard input.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// Must be [`crate::contract::CONTRACT`].
    pub contract: u32,
    /// A mod's folder, or a folder of mod folders.
    pub mods: PathBuf,
    /// The build id (the game's full commit) the mods are checked against.
    pub build: String,
    /// The build's `surface.json`; absent, a mod that needs it says so (`needs`).
    #[serde(default)]
    pub surface: Option<PathBuf>,
    /// The build's unpacked build data (`points.json`, `internals.json`); absent, a mod that needs it says so.
    #[serde(default)]
    pub build_data: Option<PathBuf>,
}

/// One mod's standing.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Checked {
    pub id: String,
    /// Why no build takes it, as the build log would say; then it has no verdict.
    pub refusal: Option<String>,
    pub verdict: Option<Verdict>,
    /// The file the plan didn't give that its verdict needs: `surface` or `build-data`.
    pub needs: Option<&'static str>,
}

/// The reply, one line of standard output.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Reply {
    pub contract: u32,
    /// The route a build of the usable mods takes (`prebuilt-api` or `prebuilt`), none when none is usable.
    pub route: Option<&'static str>,
    pub mods: Vec<Checked>,
}

/// What a verdict lacked: a file the plan didn't give, or one that didn't read.
enum Lack {
    Needs(&'static str),
    Unread(String),
}

/// Checks `plan`'s mods against its build.
pub fn check(plan: &Plan) -> Result<Reply, String> {
    let found = scan_bundles(&plan.mods)?;
    let mut surface: Option<Rc<mod_surface::Surface>> = None;
    let mut catalog: Option<Rc<internals::Catalog>> = None;
    let mut mods = Vec::new();
    let mut usable = Vec::new();
    for bundle in &found {
        let id = bundle.manifest.id.clone();
        let project = match api::Project::read(bundle) {
            Ok(project) => project,
            Err(error) => {
                mods.push(Checked {
                    id,
                    refusal: Some(error.to_string()),
                    verdict: None,
                    needs: None,
                });
                continue;
            }
        };
        if let Some(refusal) = prebuilt::project_refusal(&project) {
            mods.push(Checked {
                id,
                refusal: Some(refusal.plain()),
                verdict: None,
                needs: None,
            });
            continue;
        }
        let judged = judge(
            &project,
            &plan.build,
            &mut || {
                if let Some(read) = &surface {
                    return Ok(read.clone());
                }
                let path = plan.surface.as_ref().ok_or(Lack::Needs("surface"))?;
                let bytes = std::fs::read(path)
                    .map_err(|e| Lack::Unread(format!("{}: {e}", path.display())))?;
                let read = Rc::new(
                    mod_surface::Surface::read(&bytes)
                        .map_err(|e| Lack::Unread(format!("{}: {e}", path.display())))?,
                );
                surface = Some(read.clone());
                Ok(read)
            },
            &mut || {
                if let Some(read) = &catalog {
                    return Ok(read.clone());
                }
                let dir = plan.build_data.as_ref().ok_or(Lack::Needs("build-data"))?;
                let read = Rc::new(internals::Catalog::read(dir).map_err(Lack::Unread)?);
                catalog = Some(read.clone());
                Ok(read)
            },
        );
        match judged {
            Ok(verdict) => {
                if verdict.usable() {
                    usable.push(project);
                }
                mods.push(Checked {
                    id,
                    refusal: None,
                    verdict: Some(verdict),
                    needs: None,
                });
            }
            Err(Lack::Needs(what)) => mods.push(Checked {
                id,
                refusal: None,
                verdict: None,
                needs: Some(what),
            }),
            Err(Lack::Unread(why)) => return Err(why),
        }
    }
    Ok(Reply {
        contract: crate::contract::CONTRACT,
        route: (!usable.is_empty()).then(|| prebuilt::Route::of(&usable).name()),
        mods,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(mods: &Path, build: &str) -> Plan {
        Plan {
            contract: crate::contract::CONTRACT,
            mods: mods.to_owned(),
            build: build.into(),
            surface: None,
            build_data: None,
        }
    }

    #[test]
    fn mods_made_for_the_build_take_its_route_and_others_say_what_they_need() {
        let dir = crate::tests::dir("check-route");
        let project = dir.join("here");
        crate::tests::write_bundle(&project, "here", "");
        std::fs::write(
            project.join("Cargo.toml"),
            "[package]\nname='here'\nversion='0.1.0'\nedition='2024'\n[dependencies]\nsunrise-mod-api={path='.sunrise/sdk/api/crates/sunrise-mod-api'}\n",
        )
        .unwrap();
        let reply = check(&plan(&dir, crate::tests::A)).unwrap();
        assert_eq!(reply.mods[0].refusal, None);
        assert_eq!(reply.mods.len(), 1);
        assert_eq!(reply.mods[0].verdict, Some(Verdict::MadeFor));
        assert!(reply.route.is_some());
        let elsewhere = check(&plan(&dir, "another-build")).unwrap();
        assert_eq!(
            elsewhere.mods[0].verdict,
            Some(Verdict::NoList {
                made_for: crate::tests::A.to_string()
            }),
            "no uses table: made for its own build only"
        );
        assert_eq!(elsewhere.route, None);
    }
}

#[cfg(test)]
#[path = "check_tests.rs"]
mod fit_tests;
