//! Command line interface.

mod color_choice;
mod raw;

#[cfg(test)]
mod tests;

use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::process;

use anyhow::{anyhow, Context as ResultExt, Result};
use clap::{CommandFactory, Parser};

use crate::cli::raw::{Add, RawCommand, RawOpt};
use crate::config::{EditPlugin, GitReference, RawPlugin, Shell};
use crate::context::{log_error, Context, Output, Verbosity};
use crate::lock::LockMode;
use crate::util::build;

/// Parse the command line arguments.
///
/// In the event of failure it will print the error message and quit the program
/// without returning.
pub fn from_args() -> Opt {
    Opt::from_raw_opt(RawOpt::parse())
}

/// Resolved command line options with defaults set.
#[derive(Debug)]
pub struct Opt {
    /// Global context for use across the entire program.
    pub ctx: Context,
    /// The subcommand.
    pub command: Command,
}

/// The resolved command.
#[derive(Debug)]
pub enum Command {
    /// Initialize a new config file.
    Init { shell: Option<Shell> },
    /// Add a new plugin to the config file.
    Add {
        name: String,
        plugin: Box<EditPlugin>,
    },
    /// Open up the config file in the default editor.
    Edit,
    /// Remove a plugin from the config file.
    Remove { name: String },
    /// Install the plugins sources and generate the lock file.
    Lock,
    /// Generate and print out the script.
    Source,
}

impl Opt {
    fn from_raw_opt(raw_opt: RawOpt) -> Self {
        let RawOpt {
            quiet,
            non_interactive,
            verbose,
            color,
            data_dir,
            config_dir,
            config_file,
            profile,
            command,
        } = raw_opt;

        let mut lock_mode = None;

        let command = match command {
            RawCommand::Init { shell } => Command::Init { shell },
            RawCommand::Add(add) => {
                let (name, plugin) = EditPlugin::from_add(*add);
                Command::Add {
                    name,
                    plugin: Box::new(plugin),
                }
            }
            RawCommand::Edit => Command::Edit,
            RawCommand::Remove { name } => Command::Remove { name },
            RawCommand::Lock { update, reinstall } => {
                lock_mode = LockMode::from_lock_flags(update, reinstall);
                Command::Lock
            }
            RawCommand::Source {
                relock,
                update,
                reinstall,
            } => {
                lock_mode = LockMode::from_source_flags(relock, update, reinstall);
                Command::Source
            }
            RawCommand::Completions { shell } => {
                let mut app = RawOpt::command();
                clap_complete::generate(shell, &mut app, build::CRATE_NAME, &mut io::stdout());
                process::exit(0);
            }
            RawCommand::Version => {
                println!("{} {}", build::CRATE_NAME, build::CRATE_VERBOSE_VERSION);
                process::exit(0);
            }
        };

        let verbosity = if quiet {
            Verbosity::Quiet
        } else if verbose {
            Verbosity::Verbose
        } else {
            Verbosity::Normal
        };

        let output = Output {
            verbosity,
            no_color: !color.is_color(),
        };

        let home = match home::home_dir() {
            Some(home) => home,
            None => {
                let err = anyhow!("failed to determine the current user's home directory");
                log_error(output.no_color, &err);
                process::exit(1);
            }
        };

        let (config_dir, data_dir, config_file) =
            match resolve_paths(&home, config_dir, data_dir, config_file) {
                Ok(paths) => paths,
                Err(err) => {
                    log_error(output.no_color, &err);
                    process::exit(1);
                }
            };
        let lock_file = match profile.as_deref() {
            Some("") | None => data_dir.join("plugins.lock"),
            Some(p) => data_dir.join(format!("plugins.{p}.lock")),
        };
        let clone_dir = data_dir.join("repos");
        let download_dir = data_dir.join("downloads");

        let ctx = Context {
            version: build::CRATE_RELEASE.to_string(),
            home,
            config_dir,
            data_dir,
            config_file,
            lock_file,
            clone_dir,
            download_dir,
            profile,
            output,
            interactive: !non_interactive,
            lock_mode,
        };

        Self { ctx, command }
    }
}

impl EditPlugin {
    fn from_add(add: Add) -> (String, Self) {
        let Add {
            name,
            git,
            gist,
            github,
            remote,
            local,
            proto,
            branch,
            rev,
            tag,
            dir,
            uses,
            apply,
            profiles,
            hooks,
        } = add;

        let hooks = hooks.map(|h| h.into_iter().collect());

        let reference = match (branch, rev, tag) {
            (Some(s), None, None) => Some(GitReference::Branch(s)),
            (None, Some(s), None) => Some(GitReference::Rev(s)),
            (None, None, Some(s)) => Some(GitReference::Tag(s)),
            (None, None, None) => None,
            // this is unreachable because these three options are in the same mutually exclusive
            // 'git-reference' CLI group
            _ => unreachable!(),
        };

        (
            name,
            Self::from(RawPlugin {
                git,
                gist,
                github,
                remote,
                local,
                inline: None,
                proto,
                reference,
                dir,
                uses,
                apply,
                profiles,
                hooks,
                rest: None,
            }),
        )
    }
}

impl LockMode {
    fn from_lock_flags(update: bool, reinstall: bool) -> Option<Self> {
        match (update, reinstall) {
            (false, false) => Some(Self::Normal),
            (true, false) => Some(Self::Update),
            (false, true) => Some(Self::Reinstall),
            (true, true) => unreachable!(),
        }
    }

    fn from_source_flags(relock: bool, update: bool, reinstall: bool) -> Option<Self> {
        match (relock, update, reinstall) {
            (false, false, false) => None,
            (true, false, false) => Some(Self::Normal),
            (_, true, false) => Some(Self::Update),
            (_, false, true) => Some(Self::Reinstall),
            (_, true, true) => unreachable!(),
        }
    }
}

fn resolve_paths(
    home: &Path,
    config_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    config_file: Option<PathBuf>,
) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let (config_dir, config_file) = match (config_dir, config_file) {
        // If both are set, then use them as is
        (Some(dir), Some(file)) => (dir, file),
        // If only the config file is set, then derive the directory from the file
        (None, Some(file)) => {
            let dir = file
                .parent()
                .with_context(|| {
                    format!(
                        "failed to get parent directory of config file path `{}`",
                        file.display()
                    )
                })?
                .to_path_buf();
            (dir, file)
        }
        // If only the config directory is set, then derive the file from the directory
        (Some(dir), None) => {
            let file = dir.join("plugins.toml");
            (dir, file)
        }
        // If neither are set, then use the default config directory and file
        (None, None) => {
            let dir = default_config_dir(home);
            let file = dir.join("plugins.toml");
            (dir, file)
        }
    };

    let data_dir = data_dir.unwrap_or_else(|| default_data_dir(home));

    Ok((config_dir, data_dir, config_file))
}

fn default_config_dir(home: &Path) -> PathBuf {
    let mut p = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    p.push("sheldon");
    p
}

fn default_data_dir(home: &Path) -> PathBuf {
    let mut p = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    p.push("sheldon");
    p
}
