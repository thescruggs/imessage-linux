//! The address book: vCards from the folder kmessages (KPeople) reads,
//! `~/.local/share/kpeoplevcard` (`imsg sync-mac` and KDE Connect write
//! there), or the directories in `IMSG_CONTACTS_DIR` (`:`-separated; `;` on Windows).
//! Parsed once at start-up; `Ctrl+R` re-reads them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Person {
    pub name: String,
    pub phones: Vec<String>,
    pub emails: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Contacts {
    people: Vec<Person>,
    by_email: HashMap<String, usize>,
    /// All digits of a number -> person.
    by_digits: HashMap<String, usize>,
    /// Last ten digits -> person (national number without country code).
    by_last10: HashMap<String, usize>,
    pub files: usize,
}

/// `IMSG_CONTACTS_DIR` if set, else the KPeople vCard folder.
pub fn default_dirs() -> Vec<PathBuf> {
    match std::env::var("IMSG_CONTACTS_DIR") {
        Ok(v) if !v.trim().is_empty() => split_dirs(&v),
        _ => vec![imessage_common::paths::kpeople_vcard_dir()],
    }
}

/// A `PATH`-style list (`:`-separated; `;` on Windows, where `:` follows
/// the drive letter), empty entries dropped, `~/` expanded.
pub fn split_dirs(list: &str) -> Vec<PathBuf> {
    std::env::split_paths(list).filter(|p| !p.as_os_str().is_empty()).map(|p| expand_tilde(&p.to_string_lossy())).collect()
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(r) = p.strip_prefix("~/") {
        return imessage_common::paths::home_dir().unwrap_or_default().join(r);
    }
    PathBuf::from(p)
}

impl Contacts {
    pub fn load(dirs: &[PathBuf]) -> Contacts {
        let mut files = Vec::new();
        for d in dirs {
            collect_vcards(d, &mut files);
        }
        files.sort();
        let mut people = Vec::new();
        for f in &files {
            if let Ok(text) = std::fs::read_to_string(f) {
                people.extend(parse_vcards(&text));
            }
        }
        let mut c = Contacts::from_people(people);
        c.files = files.len();
        c
    }

    pub fn from_people(people: Vec<Person>) -> Contacts {
        let mut c = Contacts { people, ..Default::default() };
        for (i, p) in c.people.iter().enumerate() {
            for e in &p.emails {
                c.by_email.entry(e.to_lowercase()).or_insert(i);
            }
            for ph in &p.phones {
                let d = digits(ph);
                if d.is_empty() {
                    continue;
                }
                c.by_digits.entry(d.clone()).or_insert(i);
                if let Some(t) = last_ten(ph, &d) {
                    c.by_last10.entry(t.to_string()).or_insert(i);
                }
            }
        }
        c
    }

    pub fn people(&self) -> &[Person] {
        &self.people
    }

    pub fn is_empty(&self) -> bool {
        self.people.is_empty()
    }

    /// The contact name for an IDS URI (`mailto:…` / `tel:…`) or bare
    /// address: emails match case-insensitively, phone numbers by all
    /// their digits or by the last ten (as kmessages does).
    pub fn name_for(&self, uri: &str) -> Option<&str> {
        let addr = uri.strip_prefix("mailto:").or_else(|| uri.strip_prefix("tel:")).unwrap_or(uri).trim();
        if addr.contains('@') {
            return self.by_email.get(&addr.to_lowercase()).map(|i| self.people[*i].name.as_str());
        }
        let d = digits(addr);
        if d.is_empty() {
            return None;
        }
        if let Some(i) = self.by_digits.get(&d) {
            return Some(&self.people[*i].name);
        }
        if let Some(i) = last_ten(addr, &d).and_then(|t| self.by_last10.get(t)) {
            return Some(&self.people[*i].name);
        }
        None
    }
}

fn digits(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// The last ten digits of a number, the key that matches `+1 555 …` with
/// `(555) …`. Not for other international numbers (`+44 7911 123456`
/// must not match `+1 791-112-3456`): only without a country code, or
/// with `+1`, the only country code that starts with 1 (as kmessages).
fn last_ten<'a>(raw: &str, d: &'a str) -> Option<&'a str> {
    let international = raw.trim().starts_with('+');
    (d.len() >= 10 && (!international || d.starts_with('1'))).then(|| &d[d.len() - 10..])
}

fn collect_vcards(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_vcards(&p, out);
        } else if p.extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case("vcf") || x.eq_ignore_ascii_case("vcard")) {
            out.push(p);
        }
    }
}

fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(o) => out.push(o),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parse every vCard (2.1 / 3.0 / 4.0) in `text`. A person needs a name
/// (`FN`, else `N`, else `ORG`) and at least one phone or email.
pub fn parse_vcards(text: &str) -> Vec<Person> {
    // Unfold continuation lines (RFC 6350 3.2).
    let unfolded = text.replace("\r\n ", "").replace("\r\n\t", "").replace("\n ", "").replace("\n\t", "");
    #[derive(Default)]
    struct Card {
        fn_: Option<String>,
        n: Option<String>,
        org: Option<String>,
        phones: Vec<String>,
        emails: Vec<String>,
    }
    let mut people = Vec::new();
    let mut cur: Option<Card> = None;
    for line in unfolded.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let prop = key.split(';').next().unwrap_or("").rsplit('.').next().unwrap_or("").to_ascii_uppercase();
        match prop.as_str() {
            "BEGIN" if value.trim().eq_ignore_ascii_case("VCARD") => cur = Some(Card::default()),
            "END" => {
                if let Some(c) = cur.take()
                    && let Some(name) = c.fn_.or(c.n).or(c.org)
                    && (!c.phones.is_empty() || !c.emails.is_empty())
                {
                    people.push(Person { name, phones: c.phones, emails: c.emails });
                }
            }
            _ if cur.is_none() => {}
            "FN" => {
                let v = unescape(value).trim().to_string();
                if !v.is_empty() {
                    cur.as_mut().unwrap().fn_ = Some(v);
                }
            }
            "N" => {
                let parts: Vec<String> = value.split(';').map(|p| unescape(p).trim().to_string()).collect();
                let given = parts.get(1).cloned().unwrap_or_default();
                let family = parts.first().cloned().unwrap_or_default();
                let full = [given, family].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
                if !full.is_empty() {
                    cur.as_mut().unwrap().n = Some(full);
                }
            }
            "ORG" => {
                let v = unescape(value.split(';').next().unwrap_or("")).trim().to_string();
                if !v.is_empty() {
                    cur.as_mut().unwrap().org = Some(v);
                }
            }
            "TEL" => {
                let v = value.trim().strip_prefix("tel:").unwrap_or(value.trim()).to_string();
                let c = cur.as_mut().unwrap();
                if !v.is_empty() && !c.phones.contains(&v) {
                    c.phones.push(v);
                }
            }
            "EMAIL" => {
                let v = value.trim().strip_prefix("mailto:").unwrap_or(value.trim()).to_lowercase();
                let c = cur.as_mut().unwrap();
                if !v.is_empty() && !c.emails.contains(&v) {
                    c.emails.push(v);
                }
            }
            _ => {}
        }
    }
    people
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARDS: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nN:Anderson;Alice;;;\r\nFN:Alice Anderson\r\nTEL;TYPE=CELL:+1 (205) 555-0100\r\nEMAIL;TYPE=INTERNET:Alice@Example.com\r\nNOTE:a long\r\n  folded line\r\nEND:VCARD\r\nBEGIN:VCARD\r\nN:Smith;Bob;;;\r\nTEL:tel:5551234567\r\nEND:VCARD\r\nBEGIN:VCARD\r\nFN:No Address\r\nEND:VCARD\r\nBEGIN:VCARD\r\nORG:Acme\\, Inc;\r\nEMAIL:hello@acme.test\r\nEND:VCARD\r\n";

    #[test]
    fn parses_cards() {
        let p = parse_vcards(CARDS);
        assert_eq!(p.len(), 3, "{p:?}");
        assert_eq!(p[0].name, "Alice Anderson");
        assert_eq!(p[0].phones, vec!["+1 (205) 555-0100"]);
        assert_eq!(p[0].emails, vec!["alice@example.com"]);
        assert_eq!(p[1].name, "Bob Smith", "N fallback: given family");
        assert_eq!(p[1].phones, vec!["5551234567"], "tel: prefix stripped");
        assert_eq!(p[2].name, "Acme, Inc", "ORG fallback, unescaped");
    }

    #[test]
    fn matches_handles() {
        let c = Contacts::from_people(parse_vcards(CARDS));
        assert_eq!(c.name_for("mailto:ALICE@example.com"), Some("Alice Anderson"));
        assert_eq!(c.name_for("tel:+12055550100"), Some("Alice Anderson"), "all digits");
        assert_eq!(c.name_for("tel:2055550100"), Some("Alice Anderson"), "last ten digits");
        assert_eq!(c.name_for("tel:+15551234567"), Some("Bob Smith"), "stored without country code");
        assert_eq!(c.name_for("tel:46339"), None, "short codes never match by suffix");
        let uk = Contacts::from_people(vec![Person { name: "Kay".into(), phones: vec!["+44 7911 123456".into()], emails: vec![] }]);
        assert_eq!(uk.name_for("tel:+447911123456"), Some("Kay"));
        assert_eq!(uk.name_for("tel:+17911123456"), None, "other country codes never match by the last ten digits");
        assert_eq!(uk.name_for("tel:7911123456"), None);
        assert_eq!(c.name_for("mailto:nobody@x.y"), None);
        assert_eq!(c.name_for("hello@acme.test"), Some("Acme, Inc"), "bare address");
    }

    #[test]
    fn loads_directories_recursively() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/a.VCF"), CARDS).unwrap();
        std::fs::write(dir.path().join("ignored.txt"), CARDS).unwrap();
        let c = Contacts::load(&[dir.path().to_path_buf(), PathBuf::from("/nonexistent/dir")]);
        assert_eq!(c.files, 1);
        assert_eq!(c.people().len(), 3);
        assert!(Contacts::load(&[]).is_empty());
    }
}
