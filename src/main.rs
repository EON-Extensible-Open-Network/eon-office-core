// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! `eon-office-core` -- run the EON Office core, and look after its data.

use eon_office_core::{db, server, Core};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage:
  eon-office-core serve   [--home DIR] [--listen 127.0.0.1:0]
  eon-office-core backup  --out FILE [--home DIR]
  eon-office-core restore --from FILE [--home DIR]
  eon-office-core verify-audit [--home DIR]
  eon-office-core home
  eon-office-core version

The home directory is --home, else EON_OFFICE_HOME, else the per-user data
directory. Applications find a running core through <home>/runtime/endpoint.json.";

fn option(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|at| args.get(at + 1))
        .cloned()
}

fn home(args: &[String]) -> Result<PathBuf, String> {
    option(args, "--home")
        .map(PathBuf::from)
        .or_else(eon_office_core::default_home)
        .ok_or_else(|| "no home directory: pass --home or set EON_OFFICE_HOME".to_owned())
}

fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("serve") => {
            let home = home(args)?;
            let listen = option(args, "--listen").unwrap_or_else(|| "127.0.0.1:0".to_owned());
            let core = Core::open(&home).map_err(|e| e.to_string())?;
            println!(
                "EON Office core {} (api {}), home {}",
                env!("CARGO_PKG_VERSION"),
                eon_office_core::manifest::API_VERSION,
                home.display()
            );
            server::serve(&core, &listen).map_err(|e| e.to_string())?;
            println!("stopped");
            Ok(())
        }
        Some("backup") => {
            let home = home(args)?;
            let out = option(args, "--out").ok_or("backup needs --out FILE")?;
            // A second connection is fine while the core runs: SQLite's
            // online backup copies a consistent snapshot.
            let conn = db::open(&server::db_path(&home)).map_err(|e| e.to_string())?;
            db::backup(&conn, &PathBuf::from(&out)).map_err(|e| e.to_string())?;
            println!("backed up to {out}");
            Ok(())
        }
        Some("restore") => {
            let home = home(args)?;
            let from = option(args, "--from").ok_or("restore needs --from FILE")?;
            if let Some(address) = server::running_address(&home) {
                return Err(format!(
                    "a core is running for this home at {address}; stop it first"
                ));
            }
            let kept = db::restore(
                &server::db_path(&home),
                &PathBuf::from(&from),
                eon_office_core::util::now_ms(),
            )
            .map_err(|e| e.to_string())?;
            println!("restored from {from}");
            if let Some(kept) = kept {
                println!("the previous database is kept at {}", kept.display());
            }
            Ok(())
        }
        Some("verify-audit") => {
            let home = home(args)?;
            let conn = db::open(&server::db_path(&home)).map_err(|e| e.to_string())?;
            let report = eon_office_core::audit::verify(&conn).map_err(|e| e.to_string())?;
            println!("{report}");
            if report["ok"] == true {
                Ok(())
            } else {
                Err("the audit chain is broken".to_owned())
            }
        }
        Some("home") => {
            println!("{}", home(args)?.display());
            Ok(())
        }
        Some("version") => {
            println!(
                "eon-office-core {} api {} schema {}",
                env!("CARGO_PKG_VERSION"),
                eon_office_core::manifest::API_VERSION,
                db::schema_version()
            );
            Ok(())
        }
        _ => Err(USAGE.to_owned()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}
