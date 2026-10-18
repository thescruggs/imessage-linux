//! ssh destinations: the daemon's `mac_host` setting and the Mac that an
//! ssh `anisette_command` reaches (`imsg doctor`, `imsg sync-mac`).

/// Whether `h` is usable as an ssh destination argument (`mac.local`,
/// `me@mac.local`, a `Host` alias, an address): not an option (no leading
/// `-`), no whitespace, quotes or shell metacharacters.
pub fn valid_host(h: &str) -> bool {
    !h.is_empty() && !h.starts_with('-') && h.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | ':' | '[' | ']' | '%'))
}

/// ssh options that take an argument (`ssh -F file`, `-oKey=v`, ...).
const WITH_ARG: &str = "BbcDEeFIiJLlmOoPpQRSWw";

/// The destination of the first `ssh` in a command line such as
/// `ssh -F ~/.ssh/config -o BatchMode=yes me@mac.local .local/bin/x`
/// (`me@mac.local`; with `-l me` and a bare host, `me@host`). `None` if
/// the command runs no ssh or the destination is not a plain host.
/// Whitespace-separated words; quoting is not interpreted.
pub fn host_of_command(cmd: &str) -> Option<String> {
    let mut words = cmd.split_whitespace().skip_while(|w| *w != "ssh" && !w.ends_with("/ssh")).skip(1);
    let mut user: Option<String> = None;
    while let Some(w) = words.next() {
        if w == "--" {
            break;
        }
        let Some(flags) = w.strip_prefix('-') else {
            let host = match (&user, w.contains('@')) {
                (Some(u), false) => format!("{u}@{w}"),
                _ => w.to_string(),
            };
            let host = host.strip_prefix("ssh://").unwrap_or(&host).to_string();
            return valid_host(&host).then_some(host);
        };
        for (i, c) in flags.char_indices() {
            if WITH_ARG.contains(c) {
                let rest = &flags[i + c.len_utf8()..];
                let value = if rest.is_empty() { words.next().map(str::to_string) } else { Some(rest.to_string()) };
                if c == 'l' {
                    user = value;
                }
                break;
            }
        }
    }
    words.next().map(str::to_string).filter(|h| valid_host(h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        for ok in ["mac.local", "me@mac.local", "bebop", "10.0.0.5", "[fe80::1%en0]", "my_mac-2"] {
            assert!(valid_host(ok), "{ok}");
        }
        for bad in ["", "-oProxyCommand=x", "mac local", "mac;rm -rf", "a'b", "$(x)", "a\tb"] {
            assert!(!valid_host(bad), "{bad:?}");
        }
    }

    #[test]
    fn host_from_command() {
        let h = |c: &str| host_of_command(c);
        assert_eq!(h("ssh -F /home/u/.ssh/config -o BatchMode=yes -o LogLevel=ERROR bebop.local .local/bin/imessage-anisette").as_deref(), Some("bebop.local"));
        assert_eq!(h("/usr/bin/ssh -oBatchMode=yes -q me@mac.local imessage-anisette").as_deref(), Some("me@mac.local"));
        assert_eq!(h("ssh -l me -p 2222 mac x").as_deref(), Some("me@mac"));
        assert_eq!(h("ssh -tt -i ~/.ssh/id mac x").as_deref(), Some("mac"));
        assert_eq!(h("ssh -- mac x").as_deref(), Some("mac"));
        assert_eq!(h("ssh ssh://mac.local x").as_deref(), Some("mac.local"));
        assert_eq!(h("env A=b ssh mac"), Some("mac".into()));
        assert_eq!(h("printf '{}'"), None);
        assert_eq!(h("ssh -F cfg"), None);
        assert_eq!(h("ssh 'quoted host' x"), None);
        // Quoting is not interpreted: an option value with spaces is
        // explicitly unsupported (no host rather than a wrong one).
        assert_eq!(h("ssh -o ProxyCommand=\"ssh -W %h:%p jump\" mac.local x"), None);
    }
}
