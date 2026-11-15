//! hyperWIN's one account: a user name and a password of its own (`hyperwin passwd`), not a
//! Windows account. It logs mstsc in (NLA), and in kRDPclient it is what is typed where a
//! token would go: the web API trades it for a token (web.rs, POST /login).
//!
//! Kept in `<state>\account.json` as the user name and the password's NT hash: NTLM (the NLA
//! path) needs exactly that hash, and nothing else needs more, so the password itself is not
//! stored. The hash is as good as the password towards this server: the file is for
//! administrators only, like the rest of the state folder.

use anyhow::Context as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct Account {
    pub user: String,
    /// MD4 of the password as UTF-16LE, in hex
    pub nt_hash: String,
}

fn path(state: &Path) -> PathBuf {
    state.join("account.json")
}

pub fn read(state: &Path) -> Option<Account> {
    serde_json::from_slice(&std::fs::read(path(state)).ok()?).ok()
}

/// Sets the account (there is one: a new name replaces the old).
pub fn set(state: &Path, user: &str, password: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!user.is_empty() && !user.contains(['\\', '@', ' ']), "a user name without \\, @ or spaces");
    anyhow::ensure!(password.chars().count() >= 8, "a password of at least 8 characters");
    std::fs::create_dir_all(state).with_context(|| format!("create {}", state.display()))?;
    let account = Account { user: user.to_owned(), nt_hash: hex(&nt_hash(password)) };
    let tmp = state.join("account.json.new");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&account)?)?;
    std::fs::rename(&tmp, path(state))?;
    Ok(())
}

/// NTLM's NT hash of a password.
pub fn nt_hash(password: &str) -> [u8; 16] {
    use md4::{Digest as _, Md4};
    let b: Vec<u8> = password.encode_utf16().flat_map(u16::to_le_bytes).collect();
    Md4::digest(&b).into()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<[u8; 16]> {
    let b: Vec<u8> = (0..s.len() / 2).filter_map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()).collect();
    b.try_into().ok()
}

/// The user name without a `DOMAIN\` in front or an `@domain` behind (mstsc sends the client
/// computer's name as the domain).
pub fn bare_user(name: &str) -> &str {
    let name = name.rsplit_once('\\').map_or(name, |(_, user)| user);
    name.split_once('@').map_or(name, |(user, _)| user)
}

/// Refused passwords, all doors together: how many, and since when.
static FAILED: Mutex<(u32, Option<Instant>)> = Mutex::new((0, None));
/// This many refusals within WINDOW refuse every password until the window ends.
const FAILS: u32 = 5;
const WINDOW: Duration = Duration::from_secs(300);

/// Too many wrong passwords lately?
pub fn locked() -> bool {
    let mut f = FAILED.lock().unwrap_or_else(|e| e.into_inner());
    if f.1.is_some_and(|since| since.elapsed() > WINDOW) {
        *f = (0, None);
    }
    f.0 >= FAILS
}

/// Is this the account's password (and, when a name is given, its name)? Wrong ones count
/// towards the lock; while locked, nothing is right.
pub fn check(state: &Path, user: Option<&str>, password: &str) -> bool {
    if locked() {
        return false;
    }
    let ok = read(state).is_some_and(|account| {
        let name_ok = user.is_none_or(|u| bare_user(u).eq_ignore_ascii_case(&account.user));
        // every byte of both hashes is looked at, whatever the outcome
        let (a, b) = (nt_hash(password), unhex(&account.nt_hash).unwrap_or([0; 16]));
        let differing = a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y));
        name_ok && differing == 0 && !password.is_empty() && b != [0; 16]
    });
    let mut f = FAILED.lock().unwrap_or_else(|e| e.into_inner());
    if ok {
        *f = (0, None);
    } else {
        f.0 += 1;
        f.1.get_or_insert_with(Instant::now);
    }
    ok
}

/// The NT hash NTLM checks a client against, when `user` is the account's name.
pub fn nla_hash(state: &Path, user: &str) -> Option<[u8; 16]> {
    let account = read(state)?;
    if !bare_user(user).eq_ignore_ascii_case(&account.user) {
        return None;
    }
    unhex(&account.nt_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_names_with_a_domain() {
        assert_eq!(bare_user("hw"), "hw");
        assert_eq!(bare_user(r"DESKTOP-1234\hw"), "hw");
        assert_eq!(bare_user(r".\hw"), "hw");
        assert_eq!(bare_user("hw@example.org"), "hw");
        assert_eq!(bare_user(r"MicrosoftAccount\hw@example.org"), "hw");
    }

    #[test]
    fn the_nt_hash() {
        // the well-known hash of "password"
        assert_eq!(hex(&nt_hash("password")), "8846f7eaee8fb117ad06bdd830b7586c");
    }

    #[test]
    fn set_and_check() {
        let dir = std::env::temp_dir().join(format!("hyperwin-account-test-{}", std::process::id()));
        assert!(set(&dir, "admin", "short").is_err());
        assert!(set(&dir, r"a\b", "long enough").is_err());
        set(&dir, "admin", "long enough").unwrap();
        assert!(check(&dir, Some(r"PC\Admin"), "long enough"));
        assert!(check(&dir, None, "long enough"));
        assert!(!check(&dir, Some("other"), "long enough"));
        assert!(nla_hash(&dir, "admin@pc").is_some());
        assert!(nla_hash(&dir, "other").is_none());
        // (the lock is shared by the whole process: leave it open for the other tests)
        assert!(check(&dir, None, "long enough"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
