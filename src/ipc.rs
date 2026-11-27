// SPDX-FileCopyrightText: 2026 Sash contributors
// SPDX-License-Identifier: MIT

//! External control: `sash open`, `sash run`, `sash toggle` and friends.
//!
//! A second launch does not start a second app. It posts a distributed
//! notification to the running instance and exits. The notification carries a
//! token that lives in a 0600 file under the config directory, so a sandboxed
//! process cannot inject commands into the terminal even though it can post
//! notifications.

use std::io::Read;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Distributed notification name. Scoped by the app's bundle identifier.
pub const NOTIFICATION: &str = "dev.sash.terminal.command";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ExternalCommand {
    Toggle,
    NewTab {
        dir: Option<PathBuf>,
    },
    Open {
        path: PathBuf,
    },
    Run {
        command: String,
        dir: Option<PathBuf>,
    },
    Settings,
}

#[derive(Serialize)]
struct Outgoing<'a> {
    token: &'a str,
    command: &'a ExternalCommand,
}

#[derive(Deserialize)]
struct Incoming {
    token: String,
    command: ExternalCommand,
}

/// Whether these arguments are for the client rather than the app or Rune's
/// headless snapshot tooling. Anything that starts with a dash (other than
/// help) belongs to the app, so `--snap`, `--script` and `--tree` still work.
pub fn wants_cli(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        None => false,
        Some("-h" | "--help" | "help") => true,
        Some(arg) => !arg.starts_with('-') && !arg.starts_with("-psn_"),
    }
}

/// Run the command-line front end. Returns the process exit code; the app
/// itself only starts when there are no arguments.
pub fn cli(args: &[String]) -> i32 {
    match parse(args) {
        Ok(None) => 0,
        Ok(Some(command)) => match send(&command) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("sash: {e}");
                1
            }
        },
        Err(e) => {
            eprintln!("sash: {e}");
            usage();
            2
        }
    }
}

fn parse(args: &[String]) -> Result<Option<ExternalCommand>, String> {
    let Some(first) = args.first().map(String::as_str) else {
        return Ok(None);
    };
    match first {
        "-h" | "--help" | "help" => {
            usage();
            Ok(None)
        }
        "toggle" => Ok(Some(ExternalCommand::Toggle)),
        "settings" => Ok(Some(ExternalCommand::Settings)),
        "new-tab" => Ok(Some(ExternalCommand::NewTab {
            dir: args.get(1).map(|path| resolve(path)),
        })),
        "open" => {
            let path = match args.get(1) {
                Some(path) => resolve(path),
                None => std::env::current_dir().map_err(|e| e.to_string())?,
            };
            Ok(Some(ExternalCommand::Open { path }))
        }
        "run" => {
            let command = args[1..].join(" ");
            if command.trim().is_empty() {
                return Err("run needs a command".into());
            }
            Ok(Some(ExternalCommand::Run { command, dir: None }))
        }
        other
            if other.starts_with('/')
                || other.starts_with('.')
                || other.starts_with('~')
                || std::path::Path::new(other).exists() =>
        {
            Ok(Some(ExternalCommand::Open {
                path: resolve(other),
            }))
        }
        other => Err(format!("unknown command: {other}")),
    }
}

/// Expand a leading tilde and make a path absolute against the caller directory.
/// The app may be launched from Finder, where a relative path means elsewhere.
fn resolve(path: &str) -> PathBuf {
    let expanded = if path == "~" {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
    } else if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(path)
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&expanded))
            .unwrap_or(expanded)
    };
    absolute.canonicalize().unwrap_or(absolute)
}

pub fn usage() {
    println!(
        "usage: sash [toggle|settings|new-tab [dir]|open [path]|run <command...>]\n\
         \n\
         With no arguments, starts the menu-bar app. Otherwise sends the command\n\
         to the instance that is already running."
    );
}

/// App startup: make sure the token exists before the listener is installed.
pub fn ensure_token() -> Result<String, String> {
    if let Some(token) = read_token() {
        return Ok(token);
    }
    let token = random_token()?;
    let dir = Config::dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let path = dir.join("ipc-token");
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(format!("{token}\n").as_bytes()) {
                    let _ = std::fs::remove_file(&path);
                    return Err(format!("could not write {}: {e}", path.display()));
                }
            }
            // Another launch won the race; use its token.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return read_token().ok_or_else(|| format!("could not read {}", path.display()));
            }
            Err(e) => return Err(format!("could not create {}: {e}", path.display())),
        }
    }
    #[cfg(not(unix))]
    std::fs::write(&path, format!("{token}\n"))
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(token)
}

fn read_token() -> Option<String> {
    let text = std::fs::read_to_string(Config::dir().join("ipc-token")).ok()?;
    let token = text.trim();
    (!token.is_empty()).then(|| token.to_string())
}

fn random_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|e| format!("could not read /dev/urandom: {e}"))?;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    Ok(out)
}

fn send(command: &ExternalCommand) -> Result<(), String> {
    let token = read_token().ok_or_else(|| {
        "no running Sash (the token appears after the app has launched once)".to_string()
    })?;
    let payload = serde_json::to_string(&Outgoing {
        token: &token,
        command,
    })
    .map_err(|e| e.to_string())?;
    post(&payload)
}

/// Windows and Linux builds have no distributed notification bridge; the app is
/// macOS-only today, and a cross-platform socket is deliberately left for later.
#[cfg(not(target_os = "macos"))]
fn post(_payload: &str) -> Result<(), String> {
    Err("sash commands are only available on macOS".into())
}

#[cfg(target_os = "macos")]
fn post(payload: &str) -> Result<(), String> {
    use objc2_foundation::{MainThreadMarker, NSDistributedNotificationCenter, NSString};

    MainThreadMarker::new().ok_or_else(|| "sash commands need the main thread".to_string())?;
    let center = NSDistributedNotificationCenter::defaultCenter();
    let name = NSString::from_str(NOTIFICATION);
    let object = NSString::from_str(payload);
    // SAFETY: the centre and strings live for the call; userInfo is nil.
    unsafe {
        center.postNotificationName_object_userInfo_deliverImmediately(
            &name,
            Some(&object),
            None,
            true,
        );
    }
    Ok(())
}

/// Listen for commands from a second launch. Called once from the app's setup.
#[cfg(target_os = "macos")]
pub fn listen() {
    use std::ptr::NonNull;
    use std::sync::OnceLock;

    use block2::RcBlock;
    use objc2_foundation::{
        NSDistributedNotificationCenter, NSNotification, NSOperationQueue, NSString,
    };

    static LISTENING: OnceLock<()> = OnceLock::new();
    if LISTENING.set(()).is_err() {
        return;
    }
    let Some(handle) = crate::handle().cloned() else {
        return;
    };
    let Some(token) = read_token() else {
        return;
    };
    let center = NSDistributedNotificationCenter::defaultCenter();
    let name = NSString::from_str(NOTIFICATION);
    let queue = NSOperationQueue::mainQueue();
    let block = RcBlock::new(move |note: NonNull<NSNotification>| {
        // SAFETY: the notification is live for the callback's duration.
        let note = unsafe { note.as_ref() };
        let Some(object) = note.object() else {
            return;
        };
        let Ok(text) = object.downcast::<NSString>() else {
            return;
        };
        let Ok(incoming) = serde_json::from_str::<Incoming>(&text.to_string()) else {
            return;
        };
        if incoming.token != token {
            return;
        }
        handle.update(move |sash, tx| sash.handle_external(incoming.command, tx));
    });
    // SAFETY: registration runs on the main thread; AppKit copies the block and
    // owns the observer token for the life of the process, so both may be leaked.
    unsafe {
        let observer = center.addObserverForName_object_queue_usingBlock(
            Some(&name),
            None,
            Some(&queue),
            &block,
        );
        std::mem::forget(observer);
    }
}

#[cfg(not(target_os = "macos"))]
pub fn listen() {}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn rune_tooling_flags_do_not_become_client_commands() {
        assert!(!wants_cli(&args(&["--snap", "out.png"])));
        assert!(!wants_cli(&args(&["--script", "demo.json"])));
        assert!(!wants_cli(&args(&["-psn_0_123"])));
        assert!(wants_cli(&args(&["open", "."])));
        assert!(wants_cli(&args(&["--help"])));
        assert!(!wants_cli(&[]));
    }

    #[test]
    fn bare_paths_open_a_tab() {
        let command = parse(&args(&["./somewhere"])).unwrap().unwrap();
        assert!(matches!(command, ExternalCommand::Open { path } if path.is_absolute()));
    }

    #[test]
    fn run_joins_its_arguments() {
        let command = parse(&args(&["run", "make", "test"])).unwrap().unwrap();
        assert!(matches!(command, ExternalCommand::Run { command, .. } if command == "make test"));
    }

    #[test]
    fn an_unknown_word_is_an_error() {
        assert!(parse(&args(&["frobnicate"])).is_err());
    }

    #[test]
    fn an_empty_run_is_an_error() {
        assert!(parse(&args(&["run"])).is_err());
    }
}
