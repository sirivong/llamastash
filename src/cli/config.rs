//! Open the active configuration file in the user's editor.

use std::{env, ffi::OsString, path::Path, process::Command};

use crate::cli::{exit_codes, Cli, CliExit, CliResult};

/// Start `$EDITOR` with the active config path and wait for it to exit.
pub fn handle(cli: &Cli) -> CliResult {
  let path = crate::config::config_path(cli.config.clone())
    .ok_or_else(|| CliExit::new(exit_codes::UNKNOWN, "could not resolve config file path"))?;
  let editor = env::var_os("EDITOR")
    .filter(|value| !value.is_empty())
    .ok_or_else(|| {
      CliExit::new(
        exit_codes::USAGE,
        "the EDITOR environment variable is not set",
      )
    })?;
  let status = editor_command(&editor, &path).status().map_err(|error| {
    CliExit::new(
      exit_codes::UNKNOWN,
      format!("could not start the editor specified by EDITOR: {error}"),
    )
  })?;
  if status.success() {
    Ok(())
  } else {
    Err(CliExit::new(
      exit_codes::UNKNOWN,
      format!("the editor specified by EDITOR exited with {status}"),
    ))
  }
}

fn editor_command(editor: &OsString, path: &Path) -> Command {
  let mut command = Command::new(editor);
  command.arg(path);
  command
}

#[cfg(test)]
mod tests {
  use std::{ffi::OsString, path::Path};

  use super::editor_command;

  #[test]
  fn editor_receives_the_config_path_as_its_only_argument() {
    let path = Path::new("/tmp/llamastash-config.yaml");
    let command = editor_command(&OsString::from("editor"), path);

    assert_eq!(command.get_program(), "editor");
    assert_eq!(command.get_args().collect::<Vec<_>>(), [path.as_os_str()]);
  }
}
