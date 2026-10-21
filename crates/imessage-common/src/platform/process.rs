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
/// rest verbatim, as Node's `child_process` does), with cmd.exe resolved by
/// [`comspec`] rather than a PATH / current-directory search, and
/// `CREATE_NO_WINDOW`: the daemon runs without a console (a logon task),
/// and without the flag every anisette run would flash up a console window
/// for cmd.exe / ssh.exe. cmd.exe does not expand `~` the way sh does, so
/// on Windows a word starting with `~/` or `~\` gets `%USERPROFILE%` first
/// ([`expand_home`]): the Linux `ssh -F ~/.ssh/config ...` line works
/// unchanged. For tokio, wrap it in `tokio::process::Command::from`.
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
        let mut c = Command::new(comspec());
        // raw_arg: cmd.exe does not parse MSVC-style argument quoting.
        let line = match crate::paths::home_dir() {
            Some(home) => expand_home(line, &home.to_string_lossy()),
            None => line.to_string(),
        };
        c.raw_arg(format!("/d /s /c \"{line}\""));
        c.creation_flags(CREATE_NO_WINDOW);
        c
    }
}

/// `CREATE_NO_WINDOW` process creation flag (winbase.h).
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `%ComSpec%` if it is an absolute path, else
/// `%SystemRoot%\System32\cmd.exe` (fallback `C:\Windows`).
#[cfg(windows)]
pub fn comspec() -> std::path::PathBuf {
    use std::path::PathBuf;
    let abs = |v: &str| std::env::var_os(v).filter(|v| !v.is_empty()).map(PathBuf::from).filter(|p| p.is_absolute());
    abs("ComSpec").unwrap_or_else(|| abs("SystemRoot").unwrap_or_else(|| PathBuf::from(r"C:\Windows")).join("System32").join("cmd.exe"))
}

/// sh-style tilde expansion for cmd.exe: a word (start of line or after
/// whitespace) that is `~` or starts with `~/` / `~\` gets `home` in place of
/// the `~`; the whole word is double-quoted when `home` contains a space
/// (and the word no quote). Other `~`s are left alone. Pure, so it is
/// tested everywhere.
pub fn expand_home(line: &str, home: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while !rest.is_empty() {
        let ws = rest.len() - rest.trim_start().len();
        out.push_str(&rest[..ws]);
        rest = &rest[ws..];
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..end];
        let expands = word == "~" || word.starts_with("~/") || word.starts_with("~\\");
        if expands {
            let w = format!("{home}{}", &word[1..]);
            if home.contains(' ') && !w.contains('"') {
                out.push('"');
                out.push_str(&w);
                out.push('"');
            } else {
                out.push_str(&w);
            }
        } else {
            out.push_str(word);
        }
        rest = &rest[end..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_expansion() {
        let h = r"C:\Users\me";
        assert_eq!(
            expand_home("ssh -F ~/.ssh/config -o BatchMode=yes bebop .local/bin/x", h),
            r"ssh -F C:\Users\me/.ssh/config -o BatchMode=yes bebop .local/bin/x"
        );
        assert_eq!(expand_home("x ~ ~\\a  a~/b ~user/c \"~/q\"", h), r#"x C:\Users\me C:\Users\me\a  a~/b ~user/c "~/q""#);
        assert_eq!(expand_home("ssh -F ~/.ssh/config m", r"C:\Users\John Smith"), r#"ssh -F "C:\Users\John Smith/.ssh/config" m"#);
        assert_eq!(expand_home("  ~/x\t", h), "  C:\\Users\\me/x\t");
        assert_eq!(expand_home("", h), "");
    }

    #[test]
    fn shell_runs_the_line() {
        let out = shell_command("echo one && echo two").output().unwrap();
        assert!(out.status.success());
        let s = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
        assert_eq!(s.split_whitespace().collect::<Vec<_>>(), ["one", "two"]);
        assert!(!shell_command("exit 3").status().unwrap().success());
    }

    #[cfg(windows)]
    #[test]
    fn comspec_is_absolute_cmd() {
        let c = comspec();
        assert!(c.is_absolute(), "{c:?}");
        assert!(c.file_name().unwrap().to_string_lossy().eq_ignore_ascii_case("cmd.exe"), "{c:?}");
    }
}
