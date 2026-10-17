//! Keep anisette credentials out of logs and error texts.
//!
//! The anisette command's stdout carries one-time passwords and device ids
//! and is never logged. Its stderr normally holds ssh chatter and our own
//! diagnostics, but a broken helper (or an ssh `-v`) could echo header
//! values there, and the last stderr line ends up in D-Bus errors and
//! `imsg doctor`. [`redact_line`] drops lines naming an anisette header and
//! masks anything that looks like a long base64 / hex token.

/// Header names whose presence makes the whole line suspect.
/// (The Mac helper's own diagnostics therefore use neutral names, e.g.
/// "missing: machine id".)
const SECRET_MARKERS: &[&str] = &["x-apple-md", "x-apple-i-md", "x-mme-device-id", "x-apple-i-srl-no"];
/// Shortest run of base64-alphabet characters treated as a token.
const MIN_TOKEN_LEN: usize = 20;
pub const REDACTED: &str = "[redacted]";

fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-')
}

fn looks_like_token(run: &str) -> bool {
    run.len() >= MIN_TOKEN_LEN
        && run.chars().any(|c| c.is_ascii_digit())
        && run.chars().any(|c| c.is_ascii_alphabetic())
}

/// `line` with credentials removed: a line mentioning an anisette header is
/// replaced as a whole; long base64-like tokens elsewhere are masked.
pub fn redact_line(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    if SECRET_MARKERS.iter().any(|m| lower.contains(m)) {
        return format!("{REDACTED} (line mentions anisette headers)");
    }
    let mut out = String::with_capacity(line.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if looks_like_token(run) {
            out.push_str(REDACTED);
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in line.chars() {
        if token_char(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// The last non-empty line of `stderr`, trimmed and redacted.
pub fn last_line(stderr: &str) -> Option<String> {
    stderr.lines().rev().map(str::trim).find(|l| !l.is_empty()).map(redact_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_lines_are_dropped() {
        for l in [
            "imessage-anisette: AOSKit returned no OTP headers ({ \"X-Apple-MD\" = AAAA; })",
            "X-Apple-I-MD-M: c29tZXRoaW5n",
            "\"x-mme-device-id\": \"0F0F\"",
            "X-Apple-I-SRL-NO: 0F0F",
        ] {
            let r = redact_line(l);
            assert!(r.starts_with(REDACTED), "{l} -> {r}");
            assert!(!r.contains("AAAA") && !r.contains("c29t") && !r.contains("0F0F"), "{r}");
        }
    }

    #[test]
    fn tokens_are_masked_and_prose_kept() {
        let otp = "AAAABQAAABBpcVb3n0Fj1t2Yx0+Q==";
        let r = redact_line(&format!("got {otp} from helper"));
        assert_eq!(r, format!("got {REDACTED} from helper"));
        let uuid = "8A1F3C2E-0B4D-4E5F-9A6B-7C8D9E0F1A2B";
        assert_eq!(redact_line(&format!("device {uuid}.")), format!("device {REDACTED}."));
        for keep in [
            "ssh: connect to host mac.local port 22: Operation timed out",
            "Warning: Permanently added 'mac.local' (ED25519) to the list of known hosts.",
            "sh: line 1: /usr/local/bin/imessage-anisette: No such file or directory",
            // the Mac helper's diagnostics survive
            "imessage-anisette: otp dictionary has 1 of 2 required keys (missing: machine id; 3 keys in total)",
            "imessage-anisette: AOSKit returned no otp dictionary",
            "",
        ] {
            assert_eq!(redact_line(keep), keep);
        }
    }

    #[test]
    fn last_line_skips_blank_and_redacts() {
        assert_eq!(last_line("a\nb  \n\n").as_deref(), Some("b"));
        assert_eq!(last_line("\n \n"), None);
        assert!(last_line("x\nX-Apple-I-MD: abc\n").unwrap().starts_with(REDACTED));
    }
}
