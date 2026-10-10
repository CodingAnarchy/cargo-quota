mod config;
mod disk;
mod prune;
mod scan;
mod worktree;

use std::env;
use std::ffi::OsString;
use std::process::{Command, ExitCode};

use clap::{Parser, Subcommand};

use config::Opts;
use prune::Mode;

#[derive(Parser)]
#[command(name = "cargo", bin_name = "cargo")]
enum CargoCli {
    Quota(Cli),
}

/// Keep the target directory under a disk-space budget by pruning the least
/// recently used build artifacts, then run cargo.
///
/// `cargo quota build --release` prunes and then runs `cargo build --release`.
/// Any cargo subcommand can be passed through the same way.
#[derive(clap::Args)]
#[command(version)]
struct Cli {
    #[command(flatten)]
    opts: Opts,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Prune down to the budget without running cargo (the default)
    Prune,
    /// Show size, budget, and what a prune would remove, without removing anything
    Status,
    /// Prune, then run `cargo <ARGS>`
    #[command(external_subcommand)]
    Cargo(Vec<OsString>),
}

fn main() -> ExitCode {
    // Cargo invokes us as `cargo-quota quota ...`; accept a bare `cargo-quota ...` too.
    let mut args: Vec<OsString> = env::args_os().collect();
    if args.get(1).is_none_or(|a| a != "quota") {
        args.insert(1, "quota".into());
    }
    let CargoCli::Quota(cli) = CargoCli::parse_from(args);

    match cli.command {
        None | Some(Cmd::Prune) => report(prune::run(&cli.opts, &[], Mode::Prune)),
        Some(Cmd::Status) => report(prune::run(&cli.opts, &[], Mode::Status)),
        Some(Cmd::Cargo(cargo_args)) => {
            // A failed prune should never stand between the user and their build.
            if let Err(e) = prune::run(&cli.opts, &cargo_args, Mode::BeforeBuild) {
                eprintln!("warning: cargo-quota: {e:#}");
            }
            run_cargo(&cargo_args)
        }
    }
}

fn report(result: anyhow::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run_cargo(args: &[OsString]) -> ExitCode {
    // `CARGO` is set when cargo invoked us, and points at the active toolchain's cargo.
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(&cargo);
    cmd.args(args);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        eprintln!("error: failed to run {}: {err}", cargo.to_string_lossy());
        ExitCode::FAILURE
    }
    #[cfg(not(unix))]
    match cmd.status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(err) => {
            eprintln!("error: failed to run {}: {err}", cargo.to_string_lossy());
            ExitCode::FAILURE
        }
    }
}
