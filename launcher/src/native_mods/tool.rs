//! The SDK's native build tool, `cargo sunrise-mod native-build`, run as a process with the launcher's toolchain (docs/modding/src/reference/sdk/08-native-build.md).
use crate::i18n::trf;
use crate::rust_toolchain::Toolchain;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;

/// The contract this launcher's plans speak.
const CONTRACT: u32 = 2;

/// The tool's program in its unpacked SDK slice.
pub(super) const PROGRAM: &str = if cfg!(windows) {
    "cargo-sunrise-mod.exe"
} else {
    "cargo-sunrise-mod"
};

/// The running tools' process ids, so a cancel stops them and every compiler they started.
static RUNNING: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// The three slice folders a plan names.
pub(super) struct Package<'a> {
    pub runtime: &'a Path,
    pub api: &'a Path,
    pub engine: &'a Path,
}

/// What a build is for.
pub(super) struct Plan<'a> {
    pub work: &'a Path,
    pub package: Package<'a>,
    /// A folder of exactly the mods to compile.
    pub mods: &'a Path,
    pub server: bool,
    pub mixins_from_source: bool,
}

/// The registry crates a build would add, which the player says yes or no to.
pub struct Consent {
    pub added_crates: usize,
    pub build_scripts: Vec<String>,
    pub proc_macros: Vec<String>,
}

/// The mod a failed build stops at, and how.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Blame {
    #[serde(rename = "mod")]
    pub mod_id: String,
    pub kind: BlameKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlameKind {
    Compile,
    BuildScript,
    Features,
}

/// What a build made.
#[derive(Debug, Deserialize)]
pub(super) struct Built {
    pub route: String,
    pub binary: PathBuf,
    pub libraries: Vec<PathBuf>,
    pub records: Value,
    pub internals: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
enum Event {
    Mod {
        id: String,
        done: usize,
        total: usize,
    },
    Consent {
        added_crates: usize,
        build_scripts: Vec<String>,
        proc_macros: Vec<String>,
    },
    Built {
        #[serde(flatten)]
        built: Built,
        used: Vec<PathBuf>,
    },
    Failed {
        error: String,
        blame: Option<Blame>,
        used: Vec<PathBuf>,
    },
}

/// Why a build failed: its error, and the mod it stops at when it names one.
#[derive(Debug)]
pub(super) struct Failure {
    pub error: String,
    pub blame: Option<Blame>,
}

impl From<String> for Failure {
    fn from(error: String) -> Self {
        Self { error, blame: None }
    }
}

/// The toolchain as a plan names it: the launcher's, as it set it up.
pub(super) fn toolchain(rust: &Toolchain) -> Value {
    serde_json::json!({
        "dir": rust.dir,
        "target": rust.host.target,
        "cargo_home": crate::rust_toolchain::cargo_home(),
        "cargo_config": rust.cargo_config(),
        "c_bin": rust.c_bin(),
    })
}

/// The plan's JSON line.
fn plan_line(rust: &Toolchain, plan: &Plan<'_>) -> String {
    serde_json::json!({
        "contract": CONTRACT,
        "work": plan.work,
        "toolchain": toolchain(rust),
        "package": {"runtime": plan.package.runtime, "api": plan.package.api, "engine": plan.package.engine},
        "mods": plan.mods,
        "target": if plan.server { "server" } else { "client" },
        "mixins_from_source": plan.mixins_from_source,
    })
    .to_string()
}

/// Compiles `plan` with the tool at `program`: each mod's start to `on_mod`, a registry question to `consent` (none refuses).
pub(super) fn build(
    rust: &Toolchain,
    program: &Path,
    plan: &Plan<'_>,
    consent: Option<&dyn Fn(&Consent) -> bool>,
    on_mod: &dyn Fn(&str, usize, usize),
) -> Result<Built, Failure> {
    run(
        command(rust, program),
        &plan_line(rust, plan),
        consent,
        on_mod,
    )
}

/// Runs the tool's `command` on the plan line `plan`.
fn run(
    mut command: std::process::Command,
    plan: &str,
    consent: Option<&dyn Fn(&Consent) -> bool>,
    on_mod: &dyn Fn(&str, usize, usize),
) -> Result<Built, Failure> {
    command
        .arg("native-build")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let (mut child, errors) = start(command)?;
    let result = talk(&mut child, plan, consent, on_mod);
    let status = child.wait();
    let errors = finish(&mut child, errors);
    super::check_cancelled()?;
    let outcome = match (result, status) {
        (Ok(Some(done)), _) => done,
        (Ok(None), Ok(status)) => Err(ended(status.code()).into()),
        (Err(error), _) | (Ok(None), Err(error)) => Err(unrun(error).into()),
    };
    if outcome.is_err() {
        log_errors(&errors);
    }
    outcome
}

/// `program` on `rust`, with the launcher's proxy setting and GitHub mirror in its environment.
pub(super) fn command(rust: &Toolchain, program: &Path) -> std::process::Command {
    let mut command = rust.command(program);
    crate::proxy::child_env(&mut command);
    command
}

/// The most of the tool's error output kept for the build log: its last lines.
const MAX_ERRORS: usize = 16 << 10;

/// The last lines of `stream`, read lossily, at most [`MAX_ERRORS`] bytes of them.
pub(super) fn tail(stream: impl std::io::Read) -> String {
    let mut kept = std::collections::VecDeque::new();
    let mut size = 0;
    for line in BufReader::new(stream).split(b'\n').map_while(Result::ok) {
        let line = String::from_utf8_lossy(&line).trim_end().to_owned();
        size += line.len() + 1;
        kept.push_back(line);
        while size > MAX_ERRORS {
            size -= kept.pop_front().map_or(0, |line| line.len() + 1);
        }
    }
    Vec::from(kept).join("\n")
}

/// Puts the end of what a failed tool wrote on stderr in the build log, where its failure message points.
pub(super) fn log_errors(errors: &str) {
    if !errors.is_empty() {
        let _ = super::append_log(&trf(
            "The native build tool's error output (its last lines):\n{output}",
            &[("output", errors)],
        ));
    }
}

/// Why the tool ended without its last line, from its exit code: 3 is a tool on another contract, 2 one that didn't take the request.
pub(super) fn ended(code: Option<i32>) -> String {
    match code {
        Some(3) => crate::i18n::tr(
            "This launcher is too old for this SDK's native build tool: update the launcher.",
        )
        .to_owned(),
        Some(2) => crate::i18n::tr(
            "The native build tool didn't accept the launcher's request: see the build log.",
        )
        .to_owned(),
        _ => trf(
            "The native build tool stopped ({status}) without saying why: see the build log.",
            &[(
                "status",
                &code.map_or_else(
                    || crate::i18n::tr("by a signal").to_owned(),
                    |c| c.to_string(),
                ),
            )],
        ),
    }
}

/// Sends the plan, answers consent questions and reads events until the last.
fn talk(
    child: &mut crate::process::owned::Owned,
    plan: &str,
    consent: Option<&dyn Fn(&Consent) -> bool>,
    on_mod: &dyn Fn(&str, usize, usize),
) -> std::io::Result<Option<Result<Built, Failure>>> {
    let mut input = child.child().stdin.take().expect("piped");
    writeln!(input, "{plan}")?;
    let output = child.child().stdout.take().expect("piped");
    for line in BufReader::new(output).lines() {
        let Ok(event) = serde_json::from_str::<Event>(&line?) else {
            continue;
        };
        match event {
            Event::Mod { id, done, total } => on_mod(&id, done, total),
            Event::Consent {
                added_crates,
                build_scripts,
                proc_macros,
            } => {
                let question = Consent {
                    added_crates,
                    build_scripts,
                    proc_macros,
                };
                let yes = consent.is_some_and(|ask| ask(&question));
                writeln!(input, "{yes}")?;
            }
            Event::Built { built, used } => {
                mark_used(&used);
                return Ok(Some(Ok(built)));
            }
            Event::Failed { error, blame, used } => {
                mark_used(&used);
                return Ok(Some(Err(Failure { error, blame })));
            }
        }
    }
    Ok(None)
}

fn mark_used(paths: &[PathBuf]) {
    for path in paths {
        let _ = crate::storage::mark_used(path);
    }
}

/// Starts the tool in its own process group, kept for [`stop`], with its stderr read into a capped tail.
fn start(
    mut command: std::process::Command,
) -> Result<
    (
        crate::process::owned::Owned,
        std::thread::JoinHandle<String>,
    ),
    String,
> {
    command.stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = crate::process::owned::Owned::spawn(&mut command).map_err(unrun)?;
    running().push(child.child().id());
    let stderr = child.child().stderr.take().expect("piped");
    Ok((child, std::thread::spawn(move || tail(stderr))))
}

/// Forgets a tool [`start`] started, which ended, and returns the tail of its stderr.
fn finish(
    child: &mut crate::process::owned::Owned,
    errors: std::thread::JoinHandle<String>,
) -> String {
    let pid = child.child().id();
    running().retain(|&running| running != pid);
    errors.join().unwrap_or_default()
}

fn running() -> std::sync::MutexGuard<'static, Vec<u32>> {
    RUNNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Stops the running tool and every compiler it started.
pub(super) fn stop() {
    for pid in running().clone() {
        // SAFETY: a signal to the tool's own process group, which it leads (`process_group(0)`).
        #[cfg(unix)]
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
        }
        #[cfg(windows)]
        let _ = crate::process::command(crate::process::taskkill())
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// `native-check`'s reply: each mod's refusal, verdict or the file it needs.
#[derive(Debug, Deserialize)]
pub(super) struct Checked {
    #[serde(default)]
    pub route: Option<String>,
    pub mods: Vec<CheckedMod>,
}

#[derive(Debug, Deserialize)]
pub(super) struct CheckedMod {
    pub id: String,
    #[serde(default)]
    pub refusal: Option<String>,
    #[serde(default)]
    pub verdict: Option<super::fit::Verdict>,
    #[serde(default)]
    pub needs: Option<String>,
}

/// Asks the tool at `program` how the mods `plan` names stand against its build (`native-check`).
pub(super) fn check(program: &Path, plan: &Value) -> Result<Checked, String> {
    let value = ask(crate::process::command(program), "native-check", plan)?;
    serde_json::from_value(value).map_err(|e| {
        trf(
            "The native build tool's answer couldn't be read: {error}. Update the launcher.",
            &[("error", &e.to_string())],
        )
    })
}

/// Runs one of the tool's one-line commands (`native-check`, `native-discover`, `native-lock`, `native-layout`) with `plan` (its `contract` filled in): its reply, or its error's message.
pub(super) fn ask(
    mut command: std::process::Command,
    name: &str,
    plan: &Value,
) -> Result<Value, String> {
    let mut plan = plan.clone();
    plan["contract"] = CONTRACT.into();
    command
        .arg(name)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let (mut child, errors) = start(command)?;
    let mut input = child.child().stdin.take().expect("piped");
    writeln!(input, "{plan}").map_err(unrun)?;
    drop(input);
    let output = child.child().stdout.take().expect("piped");
    let last = BufReader::new(output)
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.trim().is_empty())
        .last();
    let status = child.wait().map_err(unrun);
    let errors = finish(&mut child, errors);
    super::check_cancelled()?;
    let result = match (last, status?.code()) {
        (None, code @ Some(2 | 3)) => Err(ended(code)),
        (last, _) => reply(last.as_deref()),
    };
    if result.is_err() {
        log_errors(&errors);
    }
    result
}

/// A one-line command's reply: its value, its error's message, or (no line at all) a tool from before the command.
fn reply(line: Option<&str>) -> Result<Value, String> {
    let too_old = || {
        crate::i18n::tr(
            "This build's native build tool is too old for this launcher: install a newer version.",
        )
        .to_owned()
    };
    let value: Value = serde_json::from_str(line.ok_or_else(too_old)?).map_err(|_| too_old())?;
    if let Some(error) = value.get("error") {
        return Err(trf(
            "The native build tool refused: {error}",
            &[("error", error["message"].as_str().unwrap_or_default())],
        ));
    }
    Ok(value)
}

fn unrun(error: std::io::Error) -> String {
    trf(
        "The native build tool couldn't run: {error}",
        &[("error", &error.to_string())],
    )
}

#[cfg(test)]
#[path = "tool_tests.rs"]
mod tests;
