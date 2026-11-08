//! walkrelay-cli: headless Walk Relay, mostly for testing and setup.
//!
//!   walkrelay-cli make-code <endpoint> <bucket> <access-key-id> <secret>
//!   walkrelay-cli check <group-code>
//!   walkrelay-cli sync <group-code> <player-name> [--dir <save folder>]... [--watch]

use anyhow::{bail, Result};
use std::path::PathBuf;
use std::sync::Arc;
use walkrelay_core::{config, Engine, EngineOptions, GameDetector, GroupCode, R2Store};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("make-code") if args.len() == 5 => {
            let g = GroupCode {
                endpoint: args[1].clone(),
                bucket: args[2].clone(),
                key_id: args[3].clone(),
                secret: args[4].clone(),
                region: "auto".into(),
            };
            g.validate()?;
            R2Store::new(&g)?.check_access()?;
            println!("{}", g.encode());
        }
        Some("check") if args.len() == 2 => {
            R2Store::new(&GroupCode::decode(&args[1])?)?.check_access()?;
            println!("OK: the group code works.");
        }
        Some("sync") if args.len() >= 3 => {
            let group = GroupCode::decode(&args[1])?;
            let mut opts = EngineOptions::new(args[2].clone(), config::default_data_dir().join("cli"));
            let mut watch = false;
            let mut dirs = Vec::new();
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--watch" => watch = true,
                    "--dir" if i + 1 < args.len() => {
                        i += 1;
                        dirs.push(PathBuf::from(&args[i]));
                    }
                    other => bail!("unknown argument {other}"),
                }
                i += 1;
            }
            if !dirs.is_empty() {
                opts.save_dirs_override = Some(dirs);
            }
            let mut engine = Engine::new(Arc::new(R2Store::new(&group)?), opts)?;
            let mut game = GameDetector::new();
            loop {
                let running = game.is_running();
                match engine.sync_once(running) {
                    Ok(events) => {
                        for e in events {
                            println!("{}", serde_json::to_string(&e)?);
                        }
                    }
                    Err(e) => eprintln!("sync failed: {e:#}"),
                }
                if !watch {
                    println!("{}", serde_json::to_string_pretty(engine.status())?);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs(15));
            }
        }
        _ => {
            eprintln!(
                "usage:\n  walkrelay-cli make-code <endpoint> <bucket> <access-key-id> <secret>\n  walkrelay-cli check <group-code>\n  walkrelay-cli sync <group-code> <player-name> [--dir <save folder>]... [--watch]"
            );
            std::process::exit(2);
        }
    }
    Ok(())
}
