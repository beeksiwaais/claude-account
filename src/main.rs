mod account;
mod paths;
mod process;
mod state;
mod usage;

use std::env;
use std::ffi::OsString;
use std::path::Path;

use anyhow::Result;
use clap::Parser;

use account::AccountCli;
use paths::AppPaths;

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let paths = AppPaths::discover()?;
    let mut arguments = env::args_os();
    let invoked_as = arguments
        .next()
        .unwrap_or_else(|| OsString::from("claude-account"));
    let mut remaining: Vec<OsString> = arguments.collect();
    let program_name = Path::new(&invoked_as)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("claude-account");

    if program_name == "claude" {
        if remaining.first().and_then(|arg| arg.to_str()) == Some("account") {
            remaining.remove(0);
            return AccountCli::parse_from(
                std::iter::once(OsString::from("claude account")).chain(remaining),
            )
            .run(&paths);
        }

        return process::exec_active_profile(&paths, &remaining);
    }

    if remaining.first().and_then(|arg| arg.to_str()) == Some("account") {
        remaining.remove(0);
    }

    AccountCli::parse_from(std::iter::once(OsString::from("claude-account")).chain(remaining))
        .run(&paths)
}
