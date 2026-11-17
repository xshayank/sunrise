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
            "ERRORS" => script.push_str("printf 'warming up\\n\\377 unknown contract\\n' >&2\n"),
            "EXIT3" => script.push_str("exit 3\n"),
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

#[test]
fn only_the_last_error_lines_are_kept_read_lossily() {
    let kept = tail(b"first\n\xff second\r\n".as_slice());
    assert_eq!(kept, "first\n\u{fffd} second");
    let many: String = (0..2000)
        .map(|n| format!("line {n:04} {}\n", "y".repeat(40)))
        .collect();
    let kept = tail(many.as_bytes());
    assert!(kept.len() <= MAX_ERRORS && kept.ends_with(&format!("line 1999 {}", "y".repeat(40))));
}

#[cfg(unix)]
#[test]
#[ignore = "daily"]
fn a_tool_on_another_contract_says_to_update_the_launcher_and_logs_its_errors() {
    let _guard = super::super::tests::scratch_state();
    let dir = std::env::temp_dir().join(format!(
        "sunrise-native-tool-contract-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    super::super::clear_log().unwrap();
    let failed = run(fake(&dir, &["ERRORS", "EXIT3"]), "{}", None, &|_, _, _| {}).unwrap_err();
    assert!(
        failed.error.contains("update the launcher"),
        "{}",
        failed.error
    );
    let log = std::fs::read_to_string(super::super::log_path()).unwrap();
    assert!(
        log.contains("warming up\n\u{fffd} unknown contract"),
        "{log}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
#[ignore = "daily"]
fn the_tool_hears_the_launchers_proxy_and_github_mirror() {
    use std::os::unix::fs::PermissionsExt;
    let _guard = super::super::tests::scratch_state();
    let dir = std::env::temp_dir().join(format!("sunrise-native-tool-env-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    let program = dir.join("tool");
    std::fs::write(&program, "#!/bin/sh\nprintf '%s|%s|%s' \"$HTTPS_PROXY\" \"$HTTP_PROXY\" \"$SUNRISE_GITHUB_MIRROR\"\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let rust = Toolchain {
        dir: dir.clone(),
        host: crate::rust_toolchain::hosts().first().unwrap(),
    };
    crate::proxy::announce(Some("http://127.0.0.1:9".into()));
    std::env::set_var("SUNRISE_GITHUB_MIRROR", "https://mirror.invalid");
    let output = command(&rust, &program).output();
    std::env::remove_var("SUNRISE_GITHUB_MIRROR");
    crate::proxy::announce(None);
    let seen = String::from_utf8(output.unwrap().stdout).unwrap();
    assert_eq!(
        seen,
        "http://127.0.0.1:9|http://127.0.0.1:9|https://mirror.invalid"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
