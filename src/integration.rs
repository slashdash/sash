// SPDX-FileCopyrightText: 2026 Sash contributors
// SPDX-License-Identifier: MIT

//! Optional shell integration: the OSC 133 markers libghostty already parses
//! for prompt jumps, command-finished notices and the last known directory.
//!
//! The installer writes a small script under the config directory and appends a
//! guarded source line to the shell's startup file. Nothing here parses terminal
//! state; libghostty owns that.

use std::path::{Path, PathBuf};

use crate::config::Config;

/// The shells whose startup files we know how to edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    Zsh,
    Bash,
    Fish,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Installed {
    /// The block was appended now.
    Now { shell: &'static str, rc: PathBuf },
    /// The shell already sources the script.
    Already { shell: &'static str },
}

impl Installed {
    pub fn message(&self) -> String {
        match self {
            Self::Now { shell, rc } => format!(
                "Installed {shell} integration in {}. New tabs will use it.",
                rc.display()
            ),
            Self::Already { shell } => {
                format!("{shell} integration is already installed. New tabs will use it.")
            }
        }
    }
}

impl Shell {
    pub fn detect(shell: Option<&str>) -> Option<Self> {
        let name = Path::new(shell?)
            .file_name()
            .and_then(|name| name.to_str())?;
        match name {
            "zsh" => Some(Self::Zsh),
            "bash" => Some(Self::Bash),
            "fish" => Some(Self::Fish),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Zsh => "zsh",
            Self::Bash => "bash",
            Self::Fish => "fish",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Zsh => "zsh",
            Self::Bash => "bash",
            Self::Fish => "fish",
        }
    }

    /// The file the login shell reads before the first prompt, plus any text a
    /// newly created file needs. Bash login shells skip `.bashrc`, so a fresh
    /// `.bash_profile` must source it before our block, or the user's settings
    /// vanish inside Sash.
    fn rc_plan(self) -> Option<(PathBuf, &'static str)> {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        match self {
            Self::Zsh => Some((home.join(".zshrc"), "")),
            Self::Bash => {
                for name in [".bash_profile", ".bash_login", ".profile"] {
                    let path = home.join(name);
                    if path.is_file() {
                        return Some((path, ""));
                    }
                }
                let prefix = if home.join(".bashrc").is_file() {
                    BASH_PROFILE_PREFIX
                } else {
                    ""
                };
                Some((home.join(".bash_profile"), prefix))
            }
            Self::Fish => {
                let base = std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".config"));
                Some((base.join("fish").join("config.fish"), ""))
            }
        }
    }

    fn script(self) -> &'static str {
        match self {
            Self::Zsh => ZSH_SCRIPT,
            Self::Bash => BASH_SCRIPT,
            Self::Fish => FISH_SCRIPT,
        }
    }

    fn marker(self) -> String {
        format!("# >>> sash shell integration ({}) >>>", self.name())
    }

    fn end_marker(self) -> String {
        format!("# <<< sash shell integration ({}) <<<", self.name())
    }

    fn source_line(self, script: &Path) -> String {
        let path = quote(script, self);
        match self {
            Self::Zsh | Self::Bash => format!("[ -f {path} ] && . {path}"),
            Self::Fish => format!("test -f {path}; and source {path}"),
        }
    }
}

/// Install the snippet for the configured shell. The caller has already asked
/// the user (this is a button, not a startup side effect).
pub fn install(shell: Option<&str>) -> Result<Installed, String> {
    let Some(shell) = Shell::detect(shell) else {
        return Err("Shell integration supports zsh, bash and fish.".into());
    };
    let (rc, prefix) = shell
        .rc_plan()
        .ok_or_else(|| "Could not find the home directory.".to_string())?;

    let dir = Config::dir();
    let script_path = dir.join(format!("shell-integration.{}", shell.extension()));
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
    std::fs::write(&script_path, shell.script())
        .map_err(|e| format!("Could not write {}: {e}", script_path.display()))?;

    let current = std::fs::read_to_string(&rc).unwrap_or_default();
    let marker = shell.marker();
    if current.contains(&marker) {
        return Ok(Installed::Already {
            shell: shell.name(),
        });
    }

    if let Some(parent) = rc.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Could not create {}: {e}", parent.display()))?;
    }
    let mut block = String::new();
    if current.is_empty() {
        block.push_str(prefix);
    } else if !current.ends_with('\n') {
        block.push('\n');
    }
    block.push_str(&format!(
        "\n{}\n{}\n{}\n",
        marker,
        shell.source_line(&script_path),
        shell.end_marker()
    ));
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&rc)
        .map_err(|e| format!("Could not open {}: {e}", rc.display()))?;
    file.write_all(block.as_bytes())
        .map_err(|e| format!("Could not write {}: {e}", rc.display()))?;

    Ok(Installed::Now {
        shell: shell.name(),
        rc,
    })
}

/// What the settings row says: installed, not installed, or unsupported.
pub fn status(shell: Option<&str>) -> String {
    let Some(shell) = Shell::detect(shell) else {
        return "Unsupported shell".into();
    };
    let installed = shell
        .rc_plan()
        .and_then(|(rc, _)| std::fs::read_to_string(rc).ok())
        .is_some_and(|text| text.contains(&shell.marker()));
    if installed {
        format!("Installed for {}", shell.name())
    } else {
        format!("Not installed for {}", shell.name())
    }
}

fn quote(path: &Path, shell: Shell) -> String {
    let text = path.to_string_lossy();
    let escaped = match shell {
        // POSIX single quotes cannot escape a quote; leave the quote, close,
        // add an escaped quote, and reopen.
        Shell::Zsh | Shell::Bash => text.replace('\'', "'\\''"),
        // Fish's single quotes understand a backslash escape.
        Shell::Fish => text.replace('\\', "\\\\").replace('\'', "\\'"),
    };
    format!("'{escaped}'")
}

const BASH_PROFILE_PREFIX: &str = "# Created by Sash: login bash reads this file, not .bashrc.\n[ -f \"$HOME/.bashrc\" ] && . \"$HOME/.bashrc\"\n\n";

const ZSH_SCRIPT: &str = r#"# sash shell integration (zsh).
# Generated by Sash. Safe to delete; Sash only uses it for prompt marks.
if [[ -z ${__SASH_SHELL_INTEGRATION-} ]]; then
  __SASH_SHELL_INTEGRATION=1
  __sash_precmd() {
    local ret=$?
    if [[ -n ${__sash_prompt-} ]]; then
      printf '\033]133;D;%d\007' "$ret"
    fi
    __sash_prompt=1
    printf '\033]133;A\007'
  }
  __sash_preexec() {
    printf '\033]133;C\007'
  }
  autoload -Uz add-zsh-hook
  add-zsh-hook precmd __sash_precmd
  add-zsh-hook preexec __sash_preexec
  PROMPT="${PROMPT}"$'%{\e]133;B\a%}'
fi
"#;

const BASH_SCRIPT: &str = r#"# sash shell integration (bash).
# Generated by Sash. Safe to delete; Sash only uses it for prompt marks.
if [ -z "${__SASH_SHELL_INTEGRATION-}" ]; then
  __SASH_SHELL_INTEGRATION=1
  __sash_prompt=
  __sash_precmd() {
    local ret=$?
    if [ -n "$__sash_prompt" ]; then
      printf '\033]133;D;%s\007' "$ret"
    fi
    __sash_prompt=1
    printf '\033]133;A\007'
  }
  __sash_preexec() {
    if [ -n "$__sash_prompt" ]; then
      __sash_prompt=
      printf '\033]133;C\007'
    fi
  }
  if [ "$(declare -p PROMPT_COMMAND 2>/dev/null | cut -c1-11)" = "declare -a " ]; then
    PROMPT_COMMAND=(__sash_precmd "${PROMPT_COMMAND[@]}")
  elif [ -n "$PROMPT_COMMAND" ]; then
    PROMPT_COMMAND="__sash_precmd; $PROMPT_COMMAND"
  else
    PROMPT_COMMAND="__sash_precmd"
  fi
  PS1="${PS1}\[\033]133;B\007\]"
  trap '__sash_preexec' DEBUG
fi
"#;

const FISH_SCRIPT: &str = r#"# sash shell integration (fish).
# Generated by Sash. Safe to delete; Sash only uses it for prompt marks.
if not set -q __SASH_SHELL_INTEGRATION
  set -g __SASH_SHELL_INTEGRATION 1
  function __sash_prompt --on-event fish_prompt
    printf '\033]133;A\007'
  end
  function __sash_preexec --on-event fish_preexec
    printf '\033]133;C\007'
  end
  function __sash_postexec --on-event fish_postexec
    printf '\033]133;D;%s\007' $status
  end
end
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_shell_by_its_final_component() {
        assert_eq!(Shell::detect(Some("/bin/zsh")), Some(Shell::Zsh));
        assert_eq!(
            Shell::detect(Some("/usr/local/bin/fish")),
            Some(Shell::Fish)
        );
        assert_eq!(Shell::detect(Some("bash")), Some(Shell::Bash));
        assert_eq!(Shell::detect(Some("/bin/sh")), None);
        assert_eq!(Shell::detect(None), None);
    }

    #[test]
    fn every_script_emits_the_four_markers_it_can() {
        for (shell, markers) in [
            (Shell::Zsh, &["133;A", "133;B", "133;C", "133;D"][..]),
            (Shell::Bash, &["133;A", "133;B", "133;C", "133;D"][..]),
            (Shell::Fish, &["133;A", "133;C", "133;D"][..]),
        ] {
            for marker in markers {
                assert!(
                    shell.script().contains(marker),
                    "{} script lacks {marker}",
                    shell.name()
                );
            }
        }
    }

    #[test]
    fn a_quote_in_a_path_survives_the_source_line() {
        assert_eq!(
            quote(Path::new("/tmp/it's here"), Shell::Zsh),
            "'/tmp/it'\\''s here'"
        );
        assert_eq!(
            quote(Path::new("/tmp/it's here"), Shell::Fish),
            "'/tmp/it\\'s here'"
        );
    }

    #[test]
    fn the_marker_is_specific_to_the_shell() {
        assert!(Shell::Zsh.marker().contains("(zsh)"));
        assert!(Shell::Bash.marker().contains("(bash)"));
        assert_ne!(Shell::Zsh.marker(), Shell::Bash.marker());
    }

    #[test]
    fn each_script_parses_in_its_own_shell_when_available() {
        let dir = std::env::temp_dir().join(format!("sash-integration-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (shell, program, args) in [
            (Shell::Zsh, "zsh", &["-n"][..]),
            (Shell::Bash, "bash", &["-n"][..]),
            (Shell::Fish, "fish", &["--no-execute"][..]),
        ] {
            if std::process::Command::new(program)
                .arg("--version")
                .output()
                .is_err()
            {
                continue;
            }
            let path = dir.join(format!("integration.{}", shell.extension()));
            std::fs::write(&path, shell.script()).unwrap();
            let status = std::process::Command::new(program)
                .args(args)
                .arg(&path)
                .status()
                .expect("shell runs");
            assert!(status.success(), "{} rejected its script", shell.name());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
