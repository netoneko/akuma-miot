//! What every part of `kot` spells the same way: seeds, rosters, task ids,
//! and the identity file. These used to be copied between `miot`'s `rpc.rs`
//! and `kot`'s `main.rs`. There's one copy now.

use miot_keys::Identity;
use miot_primitives::TaskId;
use miot_runtime::AccountId;
use std::path::{Path, PathBuf};

pub const DIM: &str = "\x1b[2m";
pub const OFF: &str = "\x1b[0m";

/// A seed spec: a small int (a deterministic dev seed, `[n; 32]` — fine for
/// a throwaway local litter, never for a real deployment) or 64 hex chars.
pub fn parse_seed(spec: &str) -> Result<[u8; 32], String> {
    let spec = spec.trim();
    if let Ok(n) = spec.parse::<u8>() {
        return Ok([n; 32]);
    }
    let bytes = hex::decode(spec.trim_start_matches("0x")).map_err(|_| format!("seed {spec:?} is neither a small int nor 64 hex chars"))?;
    bytes.try_into().map_err(|_| format!("seed {spec:?} is not 32 bytes"))
}

/// A seed file's content: [`parse_seed`]'s spelling, whitespace-trimmed.
pub fn read_seed_file(path: &Path) -> Result<Identity, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Identity::from_seed(&parse_seed(&text)?))
}

/// One roster entry's account: `name=<seed>` (someone whose seed you hold,
/// the dev convention) or `name=pub:<64 hex>` (an account only — how a real
/// deployment's roster names the other agents without every host holding
/// every seed).
pub fn roster_entry_account(value: &str) -> Result<AccountId, String> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix("pub:") {
        return miot_keys::from_hex(hex).map_err(|e| format!("roster pub:{hex}: {e:?}"));
    }
    Ok(Identity::from_seed(&parse_seed(value)?).account())
}

/// `name=seed|pub:hex,...` → `(name, account)`, in order. Names are
/// lowercased; a tag is always matched case-insensitively.
#[derive(Clone, Debug, Default)]
pub struct Roster(pub Vec<(String, AccountId)>);

impl Roster {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut out = Vec::new();
        for p in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (n, v) = p.split_once('=').ok_or_else(|| format!("roster entry {p:?} is not name=value"))?;
            out.push((n.trim().to_ascii_lowercase(), roster_entry_account(v)?));
        }
        Ok(Roster(out))
    }

    pub fn account(&self, name: &str) -> Option<AccountId> {
        let n = name.trim().trim_start_matches('@').to_ascii_lowercase();
        self.0.iter().find(|(r, _)| *r == n).map(|(_, a)| a.clone())
    }

    pub fn name_of(&self, who: &AccountId) -> String {
        self.0.iter().find(|(_, a)| a == who).map(|(n, _)| n.clone()).unwrap_or_else(|| miot_keys::short(who))
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(n, _)| n.as_str())
    }

    /// Refuse a roster that can't be a genesis: a name or an account listed
    /// twice (two labels for one key, or one label for two), or a `root`
    /// entry that isn't `root`'s account. Once the roster is genesis state,
    /// a mistake here is a chain that has to be thrown away.
    pub fn check_genesis(&self, root: &AccountId) -> Result<(), String> {
        for (i, (n, a)) in self.0.iter().enumerate() {
            if let Some((m, _)) = self.0[..i].iter().find(|(m, b)| m == n || b == a) {
                return Err(if m == n { format!("{n} is listed twice") } else { format!("{m} and {n} are the same account") });
            }
        }
        match self.account("root") {
            Some(r) if r != *root => Err("its root entry is not --root's account".into()),
            _ => Ok(()),
        }
    }
}

/// The *seed* a roster spec gives `name`, if it gives a seed at all (a
/// `pub:` entry has none). How `--as <name>` signs as a cat on a dev litter.
pub fn roster_seed(spec: &str, name: &str) -> Option<Identity> {
    let name = name.trim().to_ascii_lowercase();
    spec.split(',').find_map(|p| {
        let (n, v) = p.split_once('=')?;
        if n.trim().to_ascii_lowercase() != name || v.trim().starts_with("pub:") {
            return None;
        }
        parse_seed(v).ok().map(|s| Identity::from_seed(&s))
    })
}

pub fn parse_task(s: &str) -> Option<TaskId> {
    let s = s.trim().trim_start_matches('t');
    let mut it = s.split('.');
    let p: u32 = it.next()?.parse().ok()?;
    match it.next() {
        None => Some(TaskId::parent(p)),
        Some(sub) => Some(TaskId::sub(p, sub.parse().ok()?)),
    }
}

/// `~/.akuma/miot/id_ed25519.seed` — the operator's root identity. Kept at
/// its original path through the `miot`→`kot` merge on purpose: the file on
/// the operator's machine, and every node's `MIOT_ROOT_PUBKEY`, already
/// point at it.
pub fn root_identity_path() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    Path::new(&home).join(".akuma/miot/id_ed25519.seed")
}

/// Load the identity at `path`, creating one on first use. Real randomness
/// (`getrandom`), 0600, 64 hex chars. Also (re)writes the public half next
/// to it (`id_ed25519.pub` beside `…seed`, `<name>.pub` beside
/// `<name>.seed`) as an `authorized_keys` line — what `MIOT_ROOT_PUBKEY`
/// reads — whenever it's missing or stale.
///
/// The same mechanism generates each agent's own identity once
/// (`kot id --seed-file …`); nothing regenerates one that exists.
pub fn load_or_create_identity(path: &Path, comment: &str) -> Identity {
    let identity = match read_seed_file(path) {
        Ok(id) => id,
        Err(_) if !path.exists() => {
            let mut seed = [0u8; 32];
            getrandom::getrandom(&mut seed).expect("system RNG");
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).expect("create identity directory");
            }
            std::fs::write(path, hex::encode(seed)).expect("write identity seed");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
            eprintln!("  {DIM}new identity generated and saved to {}{OFF}", path.display());
            Identity::from_seed(&seed)
        }
        Err(e) => panic!("identity file exists but is unreadable: {e}"),
    };

    let pub_path = path.with_extension("pub");
    let line = identity.ssh_public_line(comment);
    let stale = std::fs::read_to_string(&pub_path).map(|existing| existing.trim() != line.trim()).unwrap_or(true);
    if stale {
        std::fs::write(&pub_path, &line).expect("write public key");
        eprintln!("  {DIM}public key at {}{OFF}", pub_path.display());
    }
    identity
}

/// `~` at the front of a path, expanded — clap hands flags over verbatim.
pub fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => Path::new(&std::env::var("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(p),
    }
}

/// `MIOT_ROOT_PUBKEY`-style: an `authorized_keys` line, or 64 hex chars (an
/// `AccountId32`), or a small-int dev seed. Accepting all three in one place
/// is what lets one flag replace the old `MIOT_ROOT`/`MIOT_ROOT_PUBKEY` pair.
pub fn parse_account(spec: &str) -> Result<AccountId, String> {
    let spec = spec.trim();
    if spec.starts_with("ssh-") {
        return miot_keys::account_from_ssh(spec).map_err(|e| format!("{e:?}"));
    }
    if let Ok(n) = spec.parse::<u8>() {
        return Ok(Identity::from_seed(&[n; 32]).account());
    }
    miot_keys::from_hex(spec).map_err(|e| format!("account {spec:?}: {e:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_genesis_roster_refuses_duplicates_and_a_wrong_root() {
        let root = Identity::from_seed(&[1; 32]).account();
        assert!(Roster::parse("root=1,meow=2,tama=3").unwrap().check_genesis(&root).is_ok());
        // The fcguest double-listing `deploy.py ids` used to write: same key, two labels.
        let e = Roster::parse("root=1,mimi=4,mac-akuma-aarch64=4").unwrap().check_genesis(&root).unwrap_err();
        assert!(e.contains("same account"), "{e}");
        let e = Roster::parse("root=1,tama=2,tama=3").unwrap().check_genesis(&root).unwrap_err();
        assert!(e.contains("twice"), "{e}");
        let e = Roster::parse("root=9,tama=2").unwrap().check_genesis(&root).unwrap_err();
        assert!(e.contains("root"), "{e}");
    }
}
