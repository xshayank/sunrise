//! The game's modpack porter, `sunrise-port-pack`, run from an installed version, one `--json` command a call (docs/api/port-pack.md).
use crate::backend;
use crate::i18n::trf;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// The replies' `format` this launcher reads.
const FORMAT: u64 = 1;
/// The work folder's last status reply, as the launcher keeps it.
const STATUS_FILE: &str = "launcher-status.json";
const PROGRAM: &str = if cfg!(windows) {
    "sunrise-port-pack.exe"
} else {
    "sunrise-port-pack"
};

/// Where a log line goes.
pub type Log<'a> = &'a dyn Fn(String);

/// `pack.json`, as the porter's `import` wrote it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Pack {
    pub name: String,
    pub format: String,
    pub minecraft: String,
    pub loader: String,
    pub loader_version: String,
    pub files: Vec<Value>,
}

/// Where a mod stands, as `coverage.json` says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Ported,
    Partial,
    NotYet,
    NotNeeded,
}

impl Status {
    pub const ALL: [Status; 4] = [
        Status::Ported,
        Status::Partial,
        Status::NotYet,
        Status::NotNeeded,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Status::Ported => "ported",
            Status::Partial => "partial",
            Status::NotYet => "not yet",
            Status::NotNeeded => "not needed",
        }
    }
}

/// One file of the pack in `coverage.json`.
#[derive(Clone, Debug, Deserialize)]
pub struct Row {
    pub key: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub tier: String,
    #[serde(default)]
    pub license: String,
    pub status: Status,
    /// The technical reason, for modders.
    #[serde(default)]
    pub reason: String,
    /// The same in plain English for a player, without the dependency sentence.
    #[serde(default)]
    pub plain: String,
    /// Unported dependencies, for the launcher to say in its language.
    #[serde(default)]
    pub waiting: usize,
}

/// The headline's numbers (`summary`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Summary {
    pub needing: usize,
    pub carried: usize,
    pub partial: usize,
    pub ready: usize,
    pub blocked: usize,
    pub later: usize,
}

/// How a jar was got (`fetch.json`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum How {
    Present,
    Downloaded,
    Local,
    NeedsKey,
    NotFound,
    Failed,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Outcome {
    pub key: String,
    pub how: How,
}

/// `fetch.json`: empty before the jars were got.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Fetched {
    pub files: Vec<Outcome>,
}

impl Fetched {
    pub fn count(&self, how: How) -> usize {
        self.files.iter().filter(|o| o.how == how).count()
    }
}

/// What happened to one resource pack: `available`, `missing`, `failed`, `conflict` or `unsupported`.
#[derive(Clone, Debug, Deserialize)]
pub struct ResourceOutcome {
    pub state: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ResourceReport {
    pub files: BTreeMap<String, ResourceOutcome>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ResourceReports {
    pub staged: ResourceReport,
    pub profile: ResourceReport,
}

/// A work folder's status reply.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct WorkStatus {
    pub pack: Pack,
    pub rows: Vec<Row>,
    pub summary: Summary,
    pub fetched: Fetched,
    pub mods: Vec<PathBuf>,
    pub left_off: Vec<String>,
    pub portable: usize,
    pub resourcepacks: usize,
    pub indexed_resources: usize,
    pub datapacks: usize,
    pub resource_reports: ResourceReports,
}

/// How well a checked jar runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    Works,
    Partly,
    No,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Works => "Works",
            Verdict::Partly => "Partly",
            Verdict::No => "Doesn't run",
        }
    }
}

/// What checking one jar found (`check.json` beside its carry).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub mod_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub loader: String,
    #[serde(default)]
    pub license: String,
    pub verdict: Verdict,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(default)]
    pub skipped: Vec<String>,
    #[serde(default)]
    pub depends: Vec<String>,
    #[serde(default)]
    pub namespaces: Vec<String>,
    #[serde(default)]
    pub code: Value,
}

/// A version's porter, with its prepared resources when it has them (what a carry fits the content to).
pub struct Porter {
    program: PathBuf,
    resources: Option<PathBuf>,
}

impl Porter {
    /// The porter of installed version `tag`.
    pub fn of(tag: &str) -> Result<Self, String> {
        let resources = backend::is_prepared(tag).then(|| backend::resources(tag));
        Self::in_dir(&backend::version_dir(tag), resources).ok_or_else(|| {
            trf(
                "Sunrise {version} has no modpack porter: install a newer version.",
                &[("version", tag)],
            )
        })
    }

    /// The porter of installed version `tag`, else of the version the launcher plays (the selected one, else the newest).
    pub fn for_version(tag: Option<&str>) -> Result<Self, String> {
        Self::of(&backend::resolve_version(tag)?)
    }

    /// The porter in a game's release folder, beside its `sunrise-client`, when it ships one.
    pub fn in_dir(dir: &Path, resources: Option<PathBuf>) -> Option<Self> {
        let program = dir.join(PROGRAM);
        program.is_file().then_some(Self { program, resources })
    }

    /// Runs `args` with `--json`: the reply, or None when this porter doesn't know the command (exit 2).
    fn run(&self, args: &[&std::ffi::OsStr], log: Log) -> Result<Option<Value>, String> {
        let mut command = crate::process::command(&self.program);
        command.args(args).arg("--json");
        crate::proxy::child_env(&mut command);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = crate::process::owned::Owned::spawn(&mut command).map_err(unrun)?;
        let mut last = None;
        for text in BufReader::new(child.child().stdout.take().expect("piped")).lines() {
            let text = text.map_err(unrun)?;
            let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            match value["message"].as_str() {
                Some(message) => log(message.to_owned()),
                None if value.is_object() => last = Some(value),
                None => {}
            }
        }
        let status = child.wait().map_err(unrun)?;
        if status.code() == Some(2) {
            return Ok(None);
        }
        let value = last.ok_or_else(|| {
            trf(
                "The modpack porter stopped ({status}) without saying why.",
                &[("status", &status.to_string())],
            )
        })?;
        reply(value).map(Some)
    }

    /// Runs a command that ends with the work folder's status, and keeps that status.
    fn status_of(
        &self,
        work: &Path,
        args: &[&std::ffi::OsStr],
        log: Log,
    ) -> Result<WorkStatus, String> {
        let value = self.run(args, log)?.ok_or_else(|| too_old(&self.program))?;
        keep(work, &value)
    }

    /// Imports the pack at `path` into `work`.
    pub fn import(&self, path: &Path, work: &Path, log: Log) -> Result<WorkStatus, String> {
        self.status_of(
            work,
            &["import".as_ref(), path.as_os_str(), work.as_os_str()],
            log,
        )
    }

    /// Reads the mods' metadata (Modrinth answers by file hash; nothing is downloaded).
    pub fn inventory(&self, work: &Path, log: Log) -> Result<WorkStatus, String> {
        self.status_of(work, &["inventory".as_ref(), work.as_os_str()], log)
    }

    /// Writes the coverage report.
    pub fn report(&self, work: &Path, log: Log) -> Result<WorkStatus, String> {
        self.status_of(work, &["report".as_ref(), work.as_os_str()], log)
    }

    /// The port step: the portable mods' jars (from `from` when the player has them), carried over for this version.
    pub fn port(&self, work: &Path, from: Option<&Path>, log: Log) -> Result<WorkStatus, String> {
        let mut args = vec!["port-work".as_ref(), work.as_os_str()];
        if let Some(resources) = &self.resources {
            args.extend(["--resources".as_ref(), resources.as_os_str()]);
        }
        if let Some(from) = from {
            args.extend(["--from".as_ref(), from.as_os_str()]);
        }
        self.status_of(work, &args, log)
    }

    /// Installs the staged resource packs into the profile; a porter without the command leaves them.
    pub fn profile(&self, work: &Path, log: Log) -> Result<(), String> {
        if let Some(value) = self.run(&["profile".as_ref(), work.as_os_str()], log)? {
            keep(work, &value)?;
        }
        Ok(())
    }

    /// Reads `work` back.
    pub fn status(&self, work: &Path) -> Result<WorkStatus, String> {
        self.status_of(work, &["status".as_ref(), work.as_os_str()], &|_| {})
    }

    /// Carries `jar` over into `out/carry/` and checks what of it runs.
    pub fn check(&self, jar: &Path, out: &Path) -> Result<Check, String> {
        let value = self
            .run(
                &["check".as_ref(), jar.as_os_str(), out.as_os_str()],
                &|_| {},
            )?
            .ok_or_else(|| too_old(&self.program))?;
        serde_json::from_value(value["check"].clone()).map_err(unreadable)
    }
}

/// The reply's own error, or the reply when it is one this launcher reads.
fn reply(value: Value) -> Result<Value, String> {
    if value["format"]
        .as_u64()
        .is_none_or(|format| format > FORMAT)
    {
        return Err(crate::i18n::tr(
            "This Sunrise version's modpack porter is newer than the launcher understands: update the launcher.",
        )
        .into());
    }
    match value.get("error") {
        Some(error) => Err(error["message"]
            .as_str()
            .unwrap_or(crate::i18n::tr(
                "The modpack porter failed without saying why.",
            ))
            .to_owned()),
        None => Ok(value),
    }
}

/// Why the porter couldn't be run or read, in the launcher's language.
fn unrun(error: std::io::Error) -> String {
    trf(
        "The modpack porter couldn't run: {error}",
        &[("error", &error.to_string())],
    )
}

/// A reply of the porter's that isn't one this launcher reads.
fn unreadable(error: serde_json::Error) -> String {
    trf(
        "The modpack porter's answer couldn't be read: {error}. Update the launcher.",
        &[("error", &error.to_string())],
    )
}

fn too_old(program: &Path) -> String {
    trf(
        "{program} is too old to import modpacks for this launcher: install a newer Sunrise version.",
        &[("program", &program.display().to_string())],
    )
}

/// Keeps a status reply in `work` and reads it.
fn keep(work: &Path, value: &Value) -> Result<WorkStatus, String> {
    let status = serde_json::from_value(value.clone()).map_err(unreadable)?;
    let path = work.join(STATUS_FILE);
    fs::write(&path, value.to_string()).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(status)
}

/// The status the launcher last kept for `work`, or None when it has none.
pub fn kept(work: &Path) -> Option<WorkStatus> {
    serde_json::from_str(&fs::read_to_string(work.join(STATUS_FILE)).ok()?).ok()
}

#[cfg(test)]
#[path = "porter_tests.rs"]
mod tests;
