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

/// The running tool's process id, so a cancel stops it and every compiler it started.
static RUNNING: Mutex<Option<u32>> = Mutex::new(None);

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

/// The plan's JSON line: the toolchain as the launcher set it up.
fn plan_line(rust: &Toolchain, plan: &Plan<'_>) -> String {
    serde_json::json!({
        "contract": CONTRACT,
        "work": plan.work,
        "toolchain": {
            "dir": rust.dir,
            "target": rust.host.target,
            "cargo_home": crate::rust_toolchain::cargo_home(),
            "cargo_config": rust.cargo_config(),
            "c_bin": rust.c_bin(),
        },
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
        rust.command(program),
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
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = crate::process::owned::Owned::spawn(&mut command).map_err(unrun)?;
    *running() = Some(child.child().id());
    let result = talk(&mut child, plan, consent, on_mod);
    let status = child.wait();
    *running() = None;
    super::check_cancelled()?;
    match (result, status) {
        (Ok(Some(done)), _) => done,
        (Ok(None), Ok(status)) => Err(trf(
            "The native build tool stopped ({status}) without saying why.",
            &[("status", &status.to_string())],
        )
        .into()),
        (Err(error), _) | (Ok(None), Err(error)) => Err(unrun(error).into()),
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

fn running() -> std::sync::MutexGuard<'static, Option<u32>> {
    RUNNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Stops the running tool and every compiler it started.
pub(super) fn stop() {
    let Some(pid) = *running() else {
        return;
    };
    // SAFETY: a signal to the tool's own process group, which it leads (`process_group(0)`).
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
    }
    #[cfg(windows)]
    let _ = crate::process::command("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
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
