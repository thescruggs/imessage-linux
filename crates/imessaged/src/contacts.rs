//! Contact links: the addresses one person uses (an email and a phone
//! number on the same address-book card) share one conversation, as in
//! Messages on the iPhone.
//!
//! The cards are the vCards in the KPeople folder (`contacts_dir`,
//! default `$XDG_DATA_HOME/kpeoplevcard`, which the iCloud contacts sync
//! (`crate::hub::contacts_sync`) fills). Every one-to-one chat with a linked address is kept under the
//! person's *canonical* address: their first phone number (sorted), else
//! their first email address. Cards with the same name that share an
//! address are one person (an address book can hold a card per account:
//! iCloud, Google, …). An address on the cards of different people, or one of our
//! own, is never linked.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use imessage_common::model::{ChatId, Handle};

/// One address-book card: the name and the email addresses / phone numbers as written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Card {
    pub name: String,
    pub addresses: Vec<String>,
}

/// Which of a person's addresses stand for one conversation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContactLinks {
    /// Every linked address (normalized IDS URI) -> its person's canonical address.
    canonical: HashMap<String, String>,
}

impl ContactLinks {
    /// `cards`: addresses in any form (unusable ones are dropped). `own`:
    /// our own addresses, normalized.
    pub fn build(cards: &[Card], own: &HashSet<String>) -> ContactLinks {
        let cards = people(cards, own);
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for c in &cards {
            for u in c {
                *seen.entry(u).or_default() += 1;
            }
        }
        let mut canonical = HashMap::new();
        for c in &cards {
            let mine: Vec<&String> = c.iter().filter(|u| seen[u.as_str()] == 1).collect();
            if mine.len() < 2 {
                continue;
            }
            // Sorted: phone numbers (`tel:`) before email addresses (`mailto:`).
            let canon = mine.iter().find(|u| u.starts_with("tel:")).unwrap_or(&mine[0]).to_string();
            for u in mine {
                canonical.insert(u.clone(), canon.clone());
            }
        }
        ContactLinks { canonical }
    }

    pub fn is_empty(&self) -> bool {
        self.canonical.is_empty()
    }

    /// The chat a one-to-one chat id belongs in (itself unless linked).
    pub fn chat(&self, id: &ChatId) -> ChatId {
        if id.is_group() {
            return id.clone();
        }
        match self.canonical.get(&id.0) {
            Some(c) => ChatId(c.clone()),
            None => id.clone(),
        }
    }

    /// The person's addresses if `chat` is a linked person's chat.
    pub fn members(&self, chat: &ChatId) -> Vec<String> {
        let canon = self.chat(chat);
        let mut v: Vec<String> = self.canonical.iter().filter(|(_, c)| **c == canon.0).map(|(u, _)| u.clone()).collect();
        v.sort();
        v
    }

    /// `(alias chat, canonical chat)` for every linked address that is not
    /// its person's canonical one.
    pub fn merges(&self) -> Vec<(ChatId, ChatId)> {
        let mut v: Vec<(ChatId, ChatId)> = self.canonical.iter().filter(|(u, c)| u != c).map(|(u, c)| (ChatId(u.clone()), ChatId(c.clone()))).collect();
        v.sort();
        v
    }

    /// Number of linked people.
    pub fn people(&self) -> usize {
        self.canonical.values().collect::<HashSet<_>>().len()
    }
}

/// Each person's addresses (normalized, sorted, ours dropped): cards with
/// the same name (ignoring case) that share an address are joined.
fn people(cards: &[Card], own: &HashSet<String>) -> Vec<Vec<String>> {
    let mut by_name: HashMap<String, Vec<HashSet<String>>> = HashMap::new();
    let mut unnamed = vec![];
    for c in cards {
        let mut set: HashSet<String> = c.addresses.iter().filter_map(|a| ids_uri(a)).filter(|u| !own.contains(u)).collect();
        let name = c.name.trim().to_lowercase();
        if name.is_empty() {
            unnamed.push(set);
            continue;
        }
        let group = by_name.entry(name).or_default();
        // Join every card of this name that shares an address with this one.
        let mut i = 0;
        while i < group.len() {
            if group[i].is_disjoint(&set) {
                i += 1;
            } else {
                set.extend(group.swap_remove(i));
            }
        }
        group.push(set);
    }
    by_name
        .into_values()
        .flatten()
        .chain(unnamed)
        .map(|set| {
            let mut v: Vec<String> = set.into_iter().collect();
            v.sort();
            v
        })
        .collect()
}

/// The IDS URI for an address-book email address or phone number: phone
/// numbers without a country code are taken as North American (10 digits,
/// or 11 starting with 1), as kmessages does. `None` if it is neither.
pub fn ids_uri(address: &str) -> Option<String> {
    let a = address.trim();
    let body = a.strip_prefix("tel:").or_else(|| a.strip_prefix("TEL:")).unwrap_or(a);
    if body.contains('@') || a.to_ascii_lowercase().starts_with("mailto:") {
        return Handle::try_normalize(a);
    }
    let digits: String = body.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() || !body.chars().all(|c| c.is_ascii_digit() || matches!(c, '+' | ' ' | '-' | '.' | '(' | ')' | '/')) {
        return None;
    }
    let uri = if body.trim_start().starts_with('+') {
        format!("tel:+{digits}")
    } else if digits.len() == 10 {
        format!("tel:+1{digits}")
    } else if digits.len() == 11 && digits.starts_with('1') {
        format!("tel:+{digits}")
    } else {
        format!("tel:{digits}")
    };
    Some(uri)
}

/// What identifies the folder's current contents (file names, sizes and
/// modification times), to notice a change without reading the cards.
pub type Fingerprint = Vec<(String, u64, Option<SystemTime>)>;

fn vcf_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("vcf"))).collect();
    v.sort();
    v
}

pub fn fingerprint(dir: &Path) -> Fingerprint {
    vcf_files(dir)
        .into_iter()
        .filter_map(|p| {
            let m = std::fs::metadata(&p).ok()?;
            Some((p.file_name()?.to_string_lossy().into_owned(), m.len(), m.modified().ok()))
        })
        .collect()
}

/// Every card in the folder's `.vcf` files (unreadable files are skipped).
pub fn load_cards(dir: &Path) -> Vec<Card> {
    vcf_files(dir)
        .iter()
        .filter_map(|p| std::fs::read(p).ok())
        .flat_map(|b| imessage_common::contacts::parse_vcards(&String::from_utf8_lossy(&b)))
        .map(|p| Card { name: p.name, addresses: p.emails.into_iter().chain(p.phones).collect() })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own(v: &[&str]) -> HashSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn card(name: &str, addresses: &[&str]) -> Card {
        Card { name: name.into(), addresses: addresses.iter().map(|a| a.to_string()).collect() }
    }

    #[test]
    fn loads_cards() {
        let dir = tempfile::tempdir().unwrap();
        let text = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Pat Doe\r\nEMAIL;TYPE=INTERNET:pat@example.com\r\nTEL:+15555550142\r\nPHOTO;ENCODING=b:abc\r\n def\r\nitem1.TEL;type=CELL:(205) 555-0100\r\nEND:VCARD\r\n";
        std::fs::write(dir.path().join("a.vcf"), text).unwrap();
        std::fs::write(dir.path().join("b.txt"), text).unwrap();
        assert_eq!(load_cards(dir.path()), vec![card("Pat Doe", &["pat@example.com", "+15555550142", "(205) 555-0100"])]);
        assert_eq!(fingerprint(dir.path()).len(), 1);
    }

    #[test]
    fn address_to_uri() {
        assert_eq!(ids_uri("Pat@Example.com").as_deref(), Some("mailto:pat@example.com"));
        assert_eq!(ids_uri("+1 (555) 555-0142").as_deref(), Some("tel:+15555550142"));
        assert_eq!(ids_uri("(555) 555-0142").as_deref(), Some("tel:+15555550142"));
        assert_eq!(ids_uri("1-555-555-0142").as_deref(), Some("tel:+15555550142"));
        assert_eq!(ids_uri("tel:+44 20 7946 0000").as_deref(), Some("tel:+442079460000"));
        assert_eq!(ids_uri("22395").as_deref(), Some("tel:22395"));
        assert_eq!(ids_uri("call me"), None);
        assert_eq!(ids_uri(""), None);
    }

    #[test]
    fn links_one_person() {
        let cards = vec![card("Pat", &["pat@example.com", "+15555550142"]), card("Solo", &["solo@x.com"])];
        let l = ContactLinks::build(&cards, &own(&[]));
        let phone = ChatId::direct("tel:+15555550142");
        assert_eq!(l.chat(&ChatId::direct("pat@example.com")), phone, "the phone number is canonical");
        assert_eq!(l.chat(&phone), phone);
        assert_eq!(l.chat(&ChatId::direct("solo@x.com")), ChatId::direct("solo@x.com"));
        assert_eq!(l.members(&phone), vec!["mailto:pat@example.com".to_string(), "tel:+15555550142".into()]);
        assert_eq!(l.merges(), vec![(ChatId::direct("pat@example.com"), phone)]);
        assert_eq!(l.people(), 1);
        let g = ChatId::group("ABC");
        assert_eq!(l.chat(&g), g);
    }

    #[test]
    fn email_only_person_uses_first_email() {
        let l = ContactLinks::build(&[card("Z", &["z@x.com", "a@x.com"])], &own(&[]));
        assert_eq!(l.chat(&ChatId::direct("z@x.com")), ChatId::direct("a@x.com"));
    }

    #[test]
    fn shared_and_own_addresses_are_not_linked() {
        // A household number on two cards links nobody through it; our own
        // card links nothing.
        let cards = vec![card("Alice", &["alice@x.com", "+15550001111"]), card("Bob", &["bob@x.com", "+15550001111"]), card("Me", &["me@x.com", "+15559998888"])];
        let l = ContactLinks::build(&cards, &own(&["mailto:me@x.com", "tel:+15559998888"]));
        assert!(l.is_empty(), "{l:?}");
    }

    #[test]
    fn duplicate_cards_of_one_person_are_joined() {
        // iCloud card (phone + email) and Google card (phone, other email)
        // of the same person; a stranger with the same name stays apart.
        let cards = vec![
            card("Pat Doe", &["pat@example.com", "+15555550142"]),
            card("pat doe", &["(555) 555-0142", "pat@mail.example"]),
            card("Pat Doe", &["other@x.com", "+15550002222"]),
        ];
        let l = ContactLinks::build(&cards, &own(&[]));
        let phone = ChatId::direct("tel:+15555550142");
        assert_eq!(l.members(&phone), vec!["mailto:pat@example.com".to_string(), "mailto:pat@mail.example".into(), "tel:+15555550142".into()]);
        assert_eq!(l.chat(&ChatId::direct("other@x.com")), ChatId::direct("+15550002222"));
        assert_eq!(l.people(), 2);
    }
}
