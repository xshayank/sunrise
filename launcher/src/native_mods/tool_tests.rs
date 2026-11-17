use super::*;
use std::cell::RefCell;

/// A tool that reads the plan, then says what `events` holds, one line each; `CONSENT` waits for the answer.
#[cfg(unix)]
fn fake(dir: &Path, events: &[&str]) -> std::process::Command {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).unwrap();
    let mut script =
        String::from("#!/bin/sh\nread plan\necho \"$plan\" > \"$(dirname \"$0\")/plan\"\n");
    for event in events {
        match *event {
            "CONSENT" => script.push_str(
                "echo '{\"event\":\"consent\",\"added_crates\":2,\"build_scripts\":[\"syn\"],\"proc_macros\":[]}'\nread answer\necho \"$answer\" > \"$(dirname \"$0\")/answer\"\n",
            ),
            event => script.push_str(&format!("echo '{event}'\n")),
        }
    }
    let program = dir.join("tool");
    std::fs::write(&program, script).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::process::Command::new(program)
}

#[cfg(unix)]
#[test]
#[ignore = "daily"]
fn a_tool_run_reports_its_mods_asks_consent_and_ends_built_or_blamed() {
    let dir = std::env::temp_dir().join(format!("sunrise-native-tool-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let started = RefCell::new(Vec::new());
    let on_mod = |id: &str, done: usize, total: usize| {
        started.borrow_mut().push(format!("{id} {done}/{total}"))
    };
    let asked = RefCell::new(0);
    let consent = |question: &Consent| {
        *asked.borrow_mut() += question.added_crates;
        true
    };
    let built = run(
        fake(
            &dir,
            &[
                "not an event",
                r#"{"event":"mod","id":"greeter","done":1,"total":1}"#,
                "CONSENT",
                r#"{"event":"built","route":"prebuilt","binary":"/b/game","libraries":["/b/lib.so"],"records":[{"name":"lib"}],"mods":["greeter"],"internals":{"greeter":[]},"mixins":"abc","used":[]}"#,
            ],
        ),
        r#"{"contract":2}"#,
        Some(&consent),
        &on_mod,
    )
    .unwrap();
    assert_eq!(
        (built.route.as_str(), built.binary.as_path()),
        ("prebuilt", Path::new("/b/game"))
    );
    assert_eq!(*started.borrow(), ["greeter 1/1"]);
    assert_eq!(*asked.borrow(), 2);
    assert_eq!(
        std::fs::read_to_string(dir.join("answer")).unwrap().trim(),
        "true"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("plan")).unwrap().trim(),
        r#"{"contract":2}"#
    );

    let failed = run(
        fake(
            &dir,
            &[r#"{"event":"failed","error":"Compiling greeter failed (101)","blame":{"mod":"greeter","kind":"compile"},"used":[]}"#],
        ),
        "{}",
        None,
        &on_mod,
    )
    .unwrap_err();
    assert_eq!(
        failed.blame,
        Some(Blame {
            mod_id: "greeter".into(),
            kind: BlameKind::Compile
        })
    );
    let silent = run(fake(&dir, &[]), "{}", None, &on_mod).unwrap_err();
    assert!(
        silent.error.contains("without saying why"),
        "{}",
        silent.error
    );
    let _ = std::fs::remove_dir_all(&dir);
}
