// SPDX-License-Identifier: AGPL-3.0-only
//! Display name rules (steelcore.v1 `PlayerService`). Pure functions, no
//! storage: a store gathers the facts about a name ([`NameFacts`]) and
//! [`decide`] turns them into a verdict, so every store applies the same rules.
//!
//! * Syntax: classic gamertag rules – 1..15 characters, letters A-Z a-z,
//!   digits and single spaces, starts with a letter, no space at either end.
//! * Unique across all providers, compared case-insensitively ([`name_key`]).
//! * Look-alikes count as taken ([`skeleton`]): case, I/l/1, O/0, rn/m, vv/w
//!   and spaces are folded, so "AIice", "A1ice" and "Al ice" all collide with
//!   "Alice". The folding follows the pilot name rules the Steelbound provider
//!   already uses for its own accounts.
//! * Blocked terms are compared on the skeleton.
//! * After a change, the next change is allowed after `change_interval`; the
//!   old name stays reserved for its previous owner for `reservation`.

use uuid::Uuid;

pub const MAX_LEN: usize = 15;

/// Verdict about a display name; mirrors `steelcore.v1.DisplayNameStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameStatus {
    Ok,
    InvalidLength,
    InvalidCharacters,
    Taken,
    Similar,
    Reserved,
    Blocked,
    ChangeLimit,
    Unchanged,
}

impl NameStatus {
    /// Short machine name, used in audit rows.
    pub fn as_str(self) -> &'static str {
        match self {
            NameStatus::Ok => "ok",
            NameStatus::InvalidLength => "invalid_length",
            NameStatus::InvalidCharacters => "invalid_characters",
            NameStatus::Taken => "taken",
            NameStatus::Similar => "similar",
            NameStatus::Reserved => "reserved",
            NameStatus::Blocked => "blocked",
            NameStatus::ChangeLimit => "change_limit",
            NameStatus::Unchanged => "unchanged",
        }
    }
}

/// Name policy (configurable per server).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamePolicy {
    /// Minimum seconds between two changes of one player's name.
    pub change_interval: i64,
    /// Seconds a given-up name stays reserved for its previous owner.
    pub reservation: i64,
}

impl Default for NamePolicy {
    fn default() -> Self {
        Self { change_interval: 7 * 86_400, reservation: 30 * 86_400 }
    }
}

/// Syntax check only.
pub fn check_syntax(name: &str) -> NameStatus {
    let len = name.chars().count();
    if len == 0 || len > MAX_LEN {
        return NameStatus::InvalidLength;
    }
    let b = name.as_bytes();
    if !b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b' ') {
        return NameStatus::InvalidCharacters;
    }
    if !b[0].is_ascii_alphabetic() || b[b.len() - 1] == b' ' || name.contains("  ") {
        return NameStatus::InvalidCharacters;
    }
    NameStatus::Ok
}

/// Case-insensitive uniqueness key.
pub fn name_key(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// Look-alike key: lower case, i/1 -> l, 0 -> o, spaces (and _ -) removed,
/// then rn -> m and vv -> w.
pub fn skeleton(name: &str) -> String {
    let folded: String = name
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            'i' | '1' => Some('l'),
            '0' => Some('o'),
            ' ' | '_' | '-' => None,
            c => Some(c),
        })
        .collect();
    folded.replace("rn", "m").replace("vv", "w")
}

/// A blocked term (stored folded with [`skeleton`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedTerm {
    pub skeleton: String,
    /// true: blocks every name containing the term; false: only the term itself.
    pub contains: bool,
}

pub fn is_blocked(name_skeleton: &str, terms: &[BlockedTerm]) -> bool {
    terms.iter().any(|t| {
        !t.skeleton.is_empty()
            && if t.contains { name_skeleton.contains(&t.skeleton) } else { name_skeleton == t.skeleton }
    })
}

/// What a store knows about a name, from the point of view of one player.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NameFacts {
    /// Players whose current name has the same [`name_key`].
    pub exact_owners: Vec<Uuid>,
    /// Players whose current name has the same [`skeleton`].
    pub similar_owners: Vec<Uuid>,
    /// Active reservations with the same skeleton: (player, same name_key).
    pub reservations: Vec<(Uuid, bool)>,
    /// The name matches a blocked term.
    pub blocked: bool,
}

/// The current name state of the player a check is made for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlayerNameState {
    pub player_id: Uuid,
    pub current: Option<String>,
    /// Unix seconds of the last change.
    pub changed_at: Option<i64>,
}

impl PlayerNameState {
    /// Unix seconds from which a change is allowed (0 = now / never set).
    pub fn next_change_at(&self, policy: &NamePolicy) -> i64 {
        match (&self.current, self.changed_at) {
            (Some(_), Some(t)) => t + policy.change_interval,
            _ => 0,
        }
    }
}

/// Verdict for `name`, optionally on behalf of `player`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub valid: bool,
    pub available: bool,
    pub status: NameStatus,
    /// For [`NameStatus::ChangeLimit`]: when the next change is allowed.
    pub next_change_at: i64,
}

pub fn decide(
    name: &str,
    facts: &NameFacts,
    player: Option<&PlayerNameState>,
    now: i64,
    policy: &NamePolicy,
) -> Verdict {
    let verdict = |valid, available, status, next_change_at| Verdict { valid, available, status, next_change_at };
    let syntax = check_syntax(name);
    if syntax != NameStatus::Ok {
        return verdict(false, false, syntax, 0);
    }
    let me = player.map(|p| p.player_id);
    if let Some(p) = player
        && p.current.as_deref() == Some(name)
    {
        return verdict(true, true, NameStatus::Unchanged, 0);
    }
    let other = |id: &Uuid| Some(*id) != me;

    if facts.blocked {
        return verdict(true, false, NameStatus::Blocked, 0);
    }
    if facts.exact_owners.iter().any(other) {
        return verdict(true, false, NameStatus::Taken, 0);
    }
    if facts.reservations.iter().any(|(id, exact)| *exact && other(id)) {
        return verdict(true, false, NameStatus::Reserved, 0);
    }
    if facts.similar_owners.iter().any(other) || facts.reservations.iter().any(|(id, _)| other(id)) {
        return verdict(true, false, NameStatus::Similar, 0);
    }
    if let Some(p) = player {
        let next = p.next_change_at(policy);
        if next > now {
            return verdict(true, true, NameStatus::ChangeLimit, next);
        }
    }
    verdict(true, true, NameStatus::Ok, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn syntax_follows_gamertag_rules() {
        for ok in ["A", "Alice", "Major Tom", "Pilot 7", "abcdefghijklmno", "X1 Y2 Z3"] {
            assert_eq!(check_syntax(ok), NameStatus::Ok, "{ok:?}");
        }
        for (bad, why) in [
            ("", NameStatus::InvalidLength),
            ("abcdefghijklmnop", NameStatus::InvalidLength),
            ("1Pilot", NameStatus::InvalidCharacters),
            (" Alice", NameStatus::InvalidCharacters),
            ("Alice ", NameStatus::InvalidCharacters),
            ("Major  Tom", NameStatus::InvalidCharacters),
            ("Al_ice", NameStatus::InvalidCharacters),
            ("Al-ice", NameStatus::InvalidCharacters),
            ("Alïce", NameStatus::InvalidCharacters),
            ("Alice\n", NameStatus::InvalidCharacters),
            ("Tab\tName", NameStatus::InvalidCharacters),
        ] {
            assert_eq!(check_syntax(bad), why, "{bad:?}");
        }
        // 15 non-ASCII characters: right length, wrong characters.
        assert_eq!(check_syntax("ÄÄÄÄÄÄÄÄÄÄÄÄÄÄÄ"), NameStatus::InvalidCharacters);
    }

    #[test]
    fn skeleton_folds_look_alikes() {
        let alice = skeleton("Alice");
        for s in ["AIice", "A1ice", "Al ice", "ALICE", "alice"] {
            assert_eq!(skeleton(s), alice, "{s:?}");
        }
        assert_eq!(skeleton("Rnoo"), skeleton("Moo"));
        assert_eq!(skeleton("Vvolf"), skeleton("Wolf"));
        assert_eq!(skeleton("B0b"), skeleton("Bob"));
        assert_ne!(skeleton("Bob"), skeleton("Rob"));
    }

    #[test]
    fn blocked_terms_use_the_skeleton() {
        let terms = [
            BlockedTerm { skeleton: skeleton("admin"), contains: true },
            BlockedTerm { skeleton: skeleton("system"), contains: false },
        ];
        assert!(is_blocked(&skeleton("Server Adm1n"), &terms));
        assert!(is_blocked(&skeleton("System"), &terms));
        assert!(!is_blocked(&skeleton("System Lord"), &terms));
        assert!(!is_blocked(&skeleton("Alice"), &terms));
    }

    #[test]
    fn decide_orders_reasons() {
        let p = NamePolicy::default();
        let me = PlayerNameState { player_id: id(1), current: Some("Alice".into()), changed_at: Some(1000) };
        let now = 1000 + p.change_interval + 1;
        let free = NameFacts::default();

        assert_eq!(decide("Bob", &free, None, now, &p).status, NameStatus::Ok);
        assert_eq!(decide("Alice", &free, Some(&me), now, &p).status, NameStatus::Unchanged);

        let taken = NameFacts { exact_owners: vec![id(2)], similar_owners: vec![id(2)], ..Default::default() };
        let v = decide("Bob", &taken, Some(&me), now, &p);
        assert_eq!((v.valid, v.available, v.status), (true, false, NameStatus::Taken));

        // The player's own name in another case is a change, not "taken".
        let own = NameFacts { exact_owners: vec![id(1)], similar_owners: vec![id(1)], ..Default::default() };
        assert_eq!(decide("ALICE", &own, Some(&me), now, &p).status, NameStatus::Ok);

        let similar = NameFacts { similar_owners: vec![id(2)], ..Default::default() };
        assert_eq!(decide("B0b", &similar, Some(&me), now, &p).status, NameStatus::Similar);

        let reserved = NameFacts { reservations: vec![(id(2), true)], ..Default::default() };
        assert_eq!(decide("Bob", &reserved, Some(&me), now, &p).status, NameStatus::Reserved);
        let reserved_similar = NameFacts { reservations: vec![(id(2), false)], ..Default::default() };
        assert_eq!(decide("B0b", &reserved_similar, Some(&me), now, &p).status, NameStatus::Similar);
        // Own reservation: free to take back.
        let mine = NameFacts { reservations: vec![(id(1), true)], ..Default::default() };
        assert_eq!(decide("Bob", &mine, Some(&me), now, &p).status, NameStatus::Ok);

        let blocked = NameFacts { blocked: true, ..Default::default() };
        assert_eq!(decide("Bob", &blocked, Some(&me), now, &p).status, NameStatus::Blocked);

        // Change limit, reported with the time of the next allowed change.
        let v = decide("Bob", &free, Some(&me), 1000 + 60, &p);
        assert_eq!((v.available, v.status, v.next_change_at), (true, NameStatus::ChangeLimit, 1000 + p.change_interval));
        // First name: no limit.
        let fresh = PlayerNameState { player_id: id(1), current: None, changed_at: None };
        assert_eq!(decide("Bob", &free, Some(&fresh), 0, &p).status, NameStatus::Ok);

        // Syntax comes first.
        assert_eq!(decide("1x", &taken, Some(&me), now, &p).status, NameStatus::InvalidCharacters);
    }
}
