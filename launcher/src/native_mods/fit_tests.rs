use super::*;

const OTHER_BUILD: &str = "cccccccccccccccccccccccccccccccccccccccc";

#[test]
fn at_most_five_changes_are_named_then_how_many_more() {
    let misfits: Vec<Misfit> = (0..7)
        .map(|n| Misfit {
            id: format!("fn:f{n}"),
            change: Change::Removed,
        })
        .collect();
    let said = changes(&misfits);
    assert!(said.contains("f4"), "{said}");
    assert!(!said.contains("f5"), "{said}");
    assert!(
        said.ends_with("and 2 more (all of them are in the build log)"),
        "{said}"
    );
    assert_eq!(changes(&misfits[..1]), "the function f0 (removed)");
}

#[test]
fn every_message_names_the_mod_the_builds_and_the_worlds() {
    let made_for = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned();
    let misfit = Verdict::DoesNotFit {
        made_for: made_for.clone(),
        misfits: vec![
            Misfit {
                id: "hook:player_logged_in".into(),
                change: Change::Changed,
            },
            Misfit {
                id: "method:ServerHost::give_item".into(),
                change: Change::Removed,
            },
        ],
    };
    let build = "nightly-2026-11-13 (afe471c)";
    assert_eq!(
        refusal("Greeter", &misfit, build),
        "Greeter doesn't fit this game.\nIt was made for another Sunrise version and this instance runs \
         nightly-2026-11-13 (afe471c). Since then the game changed 2 things that Greeter uses: the hook \
         player_logged_in (changed); the method ServerHost::give_item (removed).\nThe game will be \
         built without Greeter, which stays installed. Ask its author for an update made for this game."
    );
    let fits = Verdict::Fits {
        made_for: made_for.clone(),
    };
    assert_eq!(
        line("Greeter", &fits, build),
        "Made for another Sunrise version. Fits this game too."
    );
    assert!(fits.usable() && Verdict::MadeFor.usable() && !misfit.usable());
    assert_eq!(
        worlds_at_stake(
            "Greeter",
            "nightly-2026-11-20 (9a8b7c6)",
            &["Home".into(), "Test island".into()]
        ),
        "Greeter doesn't fit nightly-2026-11-20 (9a8b7c6).\n2 of your worlds use Greeter: “Home” \
         and “Test island”. Opening them without Greeter removes its blocks and what they hold, \
         for good."
    );
    assert!(worlds_at_stake("Greeter", build, &["Home".into()]).contains("One of your worlds"));
}

#[test]
fn an_update_says_how_many_mods_fit_and_which_will_be_switched_off() {
    let build = "nightly-2026-11-20 (9a8b7c6)";
    assert_eq!(
        update_summary(build, 1, &[]),
        "Move to nightly-2026-11-20 (9a8b7c6)? Your native mod fits it."
    );
    assert_eq!(
        update_summary(build, 4, &[]),
        "Move to nightly-2026-11-20 (9a8b7c6)? All 4 of your native mods fit it."
    );
    let misfit = Verdict::DoesNotFit {
        made_for: OTHER_BUILD.into(),
        misfits: vec![Misfit {
            id: "hook:player_logged_in".into(),
            change: Change::Changed,
        }],
    };
    let off = [("Greeter".to_owned(), short(&misfit))];
    assert_eq!(
        update_summary(build, 4, &off),
        "Move to nightly-2026-11-20 (9a8b7c6)? 3 of your 4 native mods fit it.\nGreeter does \
         not: the hook player_logged_in (changed). It will be switched off until its author makes a version that fits."
    );
    let exact = Verdict::Exact {
        made_for: OTHER_BUILD.into(),
        why: "[[native.mixin]]".into(),
    };
    assert_eq!(short(&exact), "it changes the game's own code");
}

#[test]
fn the_worlds_buttons_and_the_stopped_mod_say_what_is_so() {
    assert_eq!(
        back_up_and_build(1, 1, 0),
        "Back up this world and build without what doesn't fit"
    );
    assert_eq!(
        back_up_and_build(3, 1, 0),
        "Back up these worlds and build without what doesn't fit"
    );
    assert_eq!(
        back_up_and_build(1, 2, 2),
        "Back up this world and build without what didn't build"
    );
    assert_eq!(
        back_up_and_build(2, 2, 1),
        "Back up these worlds and build without those mods"
    );
    assert_eq!(back_up_and_update(1), "Back up this world and update");
    assert_eq!(back_up_and_update(2), "Back up these worlds and update");
    assert_eq!(
        actions_pending("Greeter"),
        "The build is stopped until you choose under Native mod build; Greeter stays on until then."
    );
    assert!(did_not_build_pending("Greeter").contains("stays on until you choose"));
    assert!(did_not_build_last("Greeter").contains("didn't build the last time"));
}
