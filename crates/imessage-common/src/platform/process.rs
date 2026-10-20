//! Running a user-configured command line (`anisette_command`, which is
//! the user's own string: same trust as typing it into a shell).

use std::process::Command;

/// The shell [`shell_command`] runs, for error messages.
#[cfg(not(windows))]
pub const SHELL: &str = "sh";
#[cfg(windows)]
pub const SHELL: &str = "cmd.exe";

/// A [`Command`] that runs `line` through the platform shell: `sh -c line`,
/// on Windows `cmd.exe /d /s /c "line"` (no AutoRun; `/s` takes the quoted
/// rest verbatim, as Node's `child_process` does). For tokio, wrap it in
/// `tokio::process::Command::from`.
pub fn shell_command(line: &str) -> Command {
    #[cfg(not(windows))]
    {
        let mut c = Command::new(SHELL);
        c.arg("-c").arg(line);
        c
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new(SHELL);
        // raw_arg: cmd.exe does not parse MSVC-style argument quoting.
        c.raw_arg(format!("/d /s /c \"{line}\""));
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_runs_the_line() {
        let out = shell_command("echo one && echo two").output().unwrap();
        assert!(out.status.success());
        let s = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
        assert_eq!(s.split_whitespace().collect::<Vec<_>>(), ["one", "two"]);
        assert!(!shell_command("exit 3").status().unwrap().success());
    }
}
