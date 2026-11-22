//! `void-sandbox-exec`: runs inside bubblewrap, restricts itself with Landlock, then executes
//! the command. Writes are allowed only beneath the `--write` directories; abstract Unix
//! sockets and signals are scoped to the sandbox.
//!
//!   void-sandbox-exec --probe
//!   void-sandbox-exec --write DIR [--write DIR]... -- PROGRAM [ARGS]...

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, exit};

use landlock::{
    ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetError, RulesetStatus, Scope,
};

fn restrict(writes: &[PathBuf]) -> Result<RulesetStatus, RulesetError> {
    let abi = ABI::V6;
    let mut created = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))?
        .scope(Scope::AbstractUnixSocket | Scope::Signal)?
        .create()?;
    if let Ok(root) = PathFd::new("/") {
        created = created.add_rule(PathBeneath::new(root, AccessFs::from_read(abi)))?;
    }
    for w in writes {
        if let Ok(fd) = PathFd::new(w) {
            created = created.add_rule(PathBeneath::new(fd, AccessFs::from_all(abi)))?;
        }
    }
    Ok(created.restrict_self()?.ruleset)
}

fn main() {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|a| a == "--probe") {
        exit(match restrict(&[]) {
            Ok(RulesetStatus::FullyEnforced) => 0,
            Ok(_) => 2,
            Err(_) => 3,
        });
    }
    let mut writes = Vec::new();
    while args.first().is_some_and(|a| a == "--write") && args.len() >= 2 {
        writes.push(PathBuf::from(args.remove(1)));
        args.remove(0);
    }
    if args.first().is_some_and(|a| a == "--") {
        args.remove(0);
    }
    if args.is_empty() {
        eprintln!("usage: void-sandbox-exec [--probe] --write DIR... -- PROGRAM [ARGS]...");
        exit(64);
    }
    match restrict(&writes) {
        Ok(RulesetStatus::FullyEnforced) => {}
        Ok(_) => eprintln!("void-sandbox: landlock partially enforced"),
        Err(e) => eprintln!("void-sandbox: landlock unavailable: {e}"),
    }
    let err = Command::new(&args[0]).args(&args[1..]).exec();
    eprintln!(
        "void-sandbox: can't run {}: {err}",
        args[0].to_string_lossy()
    );
    exit(127);
}
