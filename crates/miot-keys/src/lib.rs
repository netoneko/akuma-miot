//! Who a cat is.
//!
//! An account is [`AccountId32`], and for ed25519 that **is** the public key —
//! not a hash of it — which is what makes the sender recoverable from a
//! signature instead of asserted beside one:
//!
//! > *"What is wanted is **address recovery**, not a name lookup: derive the
//! > sender's identity from the signature over the payload […] a forged `from`
//! > is not a policy failure, it is a signature that does not recover to anyone
//! > in the roster."*
//! > — `LITTER_WORKFLOW.md`, the litter's own open problem
//!
//! # What this crate is, and mostly is not
//!
//! Almost nothing. Keys, signatures, accounts and the whole signed-call
//! envelope come from polkadot-sdk, which we already pull:
//!
//! | need | what provides it |
//! |---|---|
//! | keypair | `sp_core::ed25519::Pair` |
//! | account | `sp_runtime::AccountId32` |
//! | signature | `sp_runtime::MultiSignature` |
//! | signed call | `UncheckedExtrinsic` |
//! | replay protection | `frame_system::CheckNonce` |
//! | chain binding | `CheckGenesis`, `CheckSpecVersion`, `CheckTxVersion` |
//! | expiry | `CheckMortality` |
//!
//! An earlier version of this crate hand-rolled that last block as an
//! `Envelope` with a domain string, a genesis field and a nonce. It worked and
//! it was tested, and it was still a worse `SignedPayload`: one nothing else
//! can read, missing mortality and version binding, with its own byte layout to
//! get subtly wrong. Deleted.
//!
//! What remains is the piece polkadot-sdk genuinely does not have: **reading an
//! OpenSSH public key**, so `root` is the operator's existing
//! `authorized_keys` identity — *"no new secret to manage"* — rather than
//! something we mint.

use base64::Engine;
use polkadot_sdk::*;

use sp_core::{ed25519, ByteArray, Pair as _};
use sp_runtime::AccountId32;

/// An ed25519 keypair. Thin wrapper over [`ed25519::Pair`] so callers get an
/// [`AccountId32`] without repeating the conversion.
pub struct Identity(ed25519::Pair);

impl Identity {
    /// Reproducible, for tests and for a litter whose membership must be the
    /// same on every run.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Identity(ed25519::Pair::from_seed(seed))
    }

    pub fn public(&self) -> ed25519::Public {
        self.0.public()
    }

    /// The account. For ed25519 this is the public key itself, which is why a
    /// verifier needs nothing but the signature and the message.
    pub fn account(&self) -> AccountId32 {
        AccountId32::new(self.0.public().0)
    }

    pub fn sign(&self, message: &[u8]) -> ed25519::Signature {
        self.0.sign(message)
    }

    pub fn pair(&self) -> &ed25519::Pair {
        &self.0
    }

    /// This identity as an `authorized_keys` line — the inverse of
    /// [`account_from_ssh`], which is what lets the parser be tested against
    /// keys generated here rather than against somebody's real one.
    pub fn ssh_public_line(&self, comment: &str) -> String {
        let key = self.0.public().0;
        let mut wire = Vec::with_capacity(51);
        wire.extend_from_slice(&11u32.to_be_bytes());
        wire.extend_from_slice(b"ssh-ed25519");
        wire.extend_from_slice(&32u32.to_be_bytes());
        wire.extend_from_slice(&key);
        format!(
            "ssh-ed25519 {} {comment}",
            base64::engine::general_purpose::STANDARD.encode(&wire)
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum KeyError {
    /// Not an `ssh-ed25519` line. RSA and ECDSA keys are refused rather than
    /// coerced — converting one would be inventing an identity.
    NotEd25519,
    Malformed(&'static str),
}

impl core::fmt::Display for KeyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            KeyError::NotEd25519 => write!(f, "not an ssh-ed25519 key"),
            KeyError::Malformed(w) => write!(f, "malformed ssh key: {w}"),
        }
    }
}

impl std::error::Error for KeyError {}

/// Parse one `authorized_keys` line into its public key.
///
/// The wire format inside the base64 is a sequence of length-prefixed strings:
/// the algorithm name, then the key. Both lengths are big-endian `u32`, and
/// every one is checked — a truncated key must be a parse error, never a
/// silently short account.
pub fn public_from_ssh(line: &str) -> Result<ed25519::Public, KeyError> {
    let b64 = line
        .split_whitespace()
        .find(|f| f.starts_with("AAAA"))
        .ok_or(KeyError::Malformed("no base64 field"))?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|_| KeyError::Malformed("bad base64"))?;

    let take = |buf: &[u8], at: usize| -> Result<(usize, usize), KeyError> {
        let end = at.checked_add(4).ok_or(KeyError::Malformed("overflow"))?;
        let len = u32::from_be_bytes(
            buf.get(at..end)
                .ok_or(KeyError::Malformed("truncated length"))?
                .try_into()
                .unwrap(),
        ) as usize;
        let stop = end.checked_add(len).ok_or(KeyError::Malformed("overflow"))?;
        if stop > buf.len() {
            return Err(KeyError::Malformed("truncated field"));
        }
        Ok((end, stop))
    };

    let (a, b) = take(&raw, 0)?;
    if &raw[a..b] != b"ssh-ed25519" {
        return Err(KeyError::NotEd25519);
    }
    let (c, d) = take(&raw, b)?;
    ed25519::Public::from_slice(&raw[c..d]).map_err(|_| KeyError::Malformed("key is not 32 bytes"))
}

/// The operator's `authorized_keys` line as an account.
pub fn account_from_ssh(line: &str) -> Result<AccountId32, KeyError> {
    Ok(AccountId32::new(public_from_ssh(line)?.0))
}

/// Read the operator's account from a public key file.
///
/// The real key lives in the operator's own `~/.akuma/ssh/id_ed25519.pub` and
/// is read at startup. It is never compiled in: a repository is the wrong place
/// to keep a person's identity, public or not.
pub fn account_from_ssh_file(path: impl AsRef<std::path::Path>) -> std::io::Result<AccountId32> {
    let text = std::fs::read_to_string(path)?;
    account_from_ssh(&text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

/// Short form for logs: the first four bytes, like a git hash.
pub fn short(a: &AccountId32) -> String {
    AsRef::<[u8]>::as_ref(a)[..4].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests;
