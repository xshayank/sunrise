//! The programs the launcher runs in the background: on Windows they open no console window
//! (the launcher has none to share), and keep the streams their caller gives them.
pub mod leases;
#[cfg(test)]
mod output_tests;
pub mod owned;
#[cfg(unix)]
pub mod supervisor;

use std::ffi::OsStr;
use std::process::Command;

/// Windows' own `taskkill`, by its full path: never one a folder on `PATH` puts first.
#[cfg(windows)]
pub fn taskkill() -> std::path::PathBuf {
    std::env::var_os("SystemRoot")
        .map_or_else(
            || std::path::PathBuf::from(r"C:\Windows"),
            std::path::PathBuf::from,
        )
        .join(r"System32\taskkill.exe")
}

pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    hide_console(&mut command);
    command
}

/// Windows' flag for a console program started without a console window. `creation_flags` sets
/// the whole value, so a caller adding its own flags includes this one
/// ([`crate::rust_toolchain::low_priority`]).
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// [`command`] for a `Command` made elsewhere.
pub fn hide_console(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}
