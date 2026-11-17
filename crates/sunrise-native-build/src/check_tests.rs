use super::*;
use std::path::PathBuf;
use sunrise_mod_package::mod_surface::uses::Change;

const OTHER_BUILD: &str = "cccccccccccccccccccccccccccccccccccccccc";

/// A surface holding free functions (id, hash).
fn surface(functions: &[(&str, &str)]) -> mod_surface::Surface {
    let functions: Vec<_> = functions
        .iter()
        .map(|(id, hash)| {
            serde_json::json!({"id": id, "hash": hash, "signature": "fn f()", "wasm": true})
        })
        .collect();
    serde_json::from_value(serde_json::json!({
        "schema": mod_surface::SCHEMA, "hash": "s", "functions": functions,
    }))
    .unwrap()
}

/// A mod made for its bundle's pin whose table (when `uses` is given) lists those items.
fn project_dir(name: &str, uses: Option<&[(&str, &str)]>, made_for: Option<&str>) -> PathBuf {
    let dir = crate::tests::dir(name);
    crate::tests::write_bundle(&dir, "greeter", "");
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname='greeter'\nversion='0.1.0'\n[dependencies]\nsunrise-mod-api={ path = '.sunrise/sdk/api/crates/sunrise-mod-api' }\n",
    )
    .unwrap();
    let pin = pin(&dir);
    if let Some(uses) = uses {
        let entries: Vec<_> = uses
            .iter()
            .map(|(id, hash)| serde_json::json!({"id": id, "hash": hash}))
            .collect();
        let table = serde_json::json!({
            "schema": uses::USES_SCHEMA,
            "made_for": made_for.unwrap_or(&pin),
            "entries": entries,
        });
        std::fs::write(dir.join(uses::USES_FILE), table.to_string()).unwrap();
    }
    dir
}

fn judged(dir: &Path, build: &str, target: &mod_surface::Surface) -> Verdict {
    let bundle = crate::bundle_in(dir).unwrap().unwrap();
    let project = api::Project::read(&bundle).unwrap();
    let target = Rc::new(target.clone());
    judge(
        &project,
        build,
        &mut || Ok::<_, String>(target.clone()),
        &mut || Err("no [[inject]] rows: never read".to_owned()),
    )
    .unwrap()
}

fn pin(dir: &Path) -> String {
    crate::bundle_in(dir)
        .unwrap()
        .unwrap()
        .native()
        .sunrise
        .clone()
}

#[test]
fn a_mod_fits_a_build_where_what_it_uses_is_unchanged() {
    let dir = project_dir("fit-fits", Some(&[("fn:give", "h1")]), None);
    // Something it doesn't use changed, and something new came: it still fits.
    let target = surface(&[("fn:give", "h1"), ("fn:other", "h9"), ("fn:new", "n")]);
    assert_eq!(
        judged(&dir, OTHER_BUILD, &target),
        Verdict::Fits {
            made_for: pin(&dir)
        }
    );
    // On the build it was made for, no test runs.
    assert_eq!(judged(&dir, &pin(&dir), &surface(&[])), Verdict::MadeFor);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_copy_a_build_makes_is_judged_by_the_same_table() {
    let dir = project_dir("fit-copied", Some(&[("fn:give", "h1")]), None);
    let bundle = crate::bundle_in(&dir).unwrap().unwrap();
    let copy = crate::tests::dir("fit-copied-to");
    crate::package::copy(&bundle, &copy).unwrap();
    let target = surface(&[("fn:give", "h1")]);
    assert_eq!(
        judged(&copy, OTHER_BUILD, &target),
        Verdict::Fits {
            made_for: pin(&dir)
        }
    );
    std::fs::remove_dir_all(dir).unwrap();
    std::fs::remove_dir_all(copy).unwrap();
}

#[test]
fn a_changed_or_removed_item_it_uses_is_named() {
    let dir = project_dir(
        "fit-misfit",
        Some(&[("fn:give", "h1"), ("hook:player_logged_in", "h2")]),
        None,
    );
    let target = surface(&[("fn:give", "h1-changed")]);
    let Verdict::DoesNotFit { made_for, misfits } = judged(&dir, OTHER_BUILD, &target) else {
        panic!("a changed item fits");
    };
    assert_eq!(made_for, pin(&dir));
    let said: Vec<(&str, &Change)> = misfits.iter().map(|m| (m.id.as_str(), &m.change)).collect();
    assert_eq!(
        said,
        [
            ("fn:give", &Change::Changed),
            ("hook:player_logged_in", &Change::Removed)
        ]
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn without_a_list_for_its_pin_a_mod_works_on_its_own_build_only() {
    let none = project_dir("fit-no-list", None, None);
    let target = surface(&[("fn:give", "h1")]);
    assert_eq!(
        judged(&none, OTHER_BUILD, &target),
        Verdict::NoList {
            made_for: pin(&none)
        }
    );
    // A list made for another build than the mod's pin says nothing about this one.
    let stale = project_dir(
        "fit-stale-list",
        Some(&[("fn:give", "h1")]),
        Some("dddddddddddddddddddddddddddddddddddddddd"),
    );
    assert_eq!(
        judged(&stale, OTHER_BUILD, &target),
        Verdict::NoList {
            made_for: pin(&stale)
        }
    );
    // A list that does not read is no list: never a panic, never "fits".
    std::fs::write(
        none.join(uses::USES_FILE),
        b"{\"schema\": 1, \"made_for\": 3}",
    )
    .unwrap();
    assert_eq!(
        judged(&none, OTHER_BUILD, &target),
        Verdict::NoList {
            made_for: pin(&none)
        }
    );
    std::fs::remove_dir_all(none).unwrap();
    std::fs::remove_dir_all(stale).unwrap();
}

#[test]
fn a_mod_that_changes_game_code_is_exact_whatever_its_list_says() {
    let dir = project_dir("fit-exact", Some(&[("fn:give", "h1")]), None);
    let manifest = dir.join(crate::NATIVE_MANIFEST);
    let text = std::fs::read_to_string(&manifest).unwrap()
        + "[[native.module]]\nid = \"m\"\nfile = \"game.rs\"\nname = \"m\"\nsource = \"src/m.rs\"\n";
    std::fs::write(&manifest, text).unwrap();
    std::fs::write(dir.join("src/m.rs"), "pub fn m() {}").unwrap();
    let target = surface(&[("fn:give", "h1")]);
    assert_eq!(
        judged(&dir, OTHER_BUILD, &target),
        Verdict::Exact {
            made_for: pin(&dir),
            why: "[[native.module]]"
        }
    );
    std::fs::remove_dir_all(dir).unwrap();
}
