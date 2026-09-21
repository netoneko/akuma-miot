//! Who a cat is.
//!
//! An identity here is an ed25519 keypair, and an **account is the public key**
//! — 32 bytes, used directly. There is no name table, no registry and no
//! mapping step, which is the entire point:
//!
//! > *"What is wanted is **address recovery**, not a name lookup: derive the
//! > sender's identity from the signature over the payload, so the identity is
//! > a consequence of the signature rather than an assertion the signature
//! > happens to sit beside. […] a forged `from` is not a policy failure, it is
//! > a signature that does not recover to anyone in the roster."*
//! > — `LITTER_WORKFLOW.md`, the litter's own open problem
//!
//! The operator's identity comes from an **existing SSH key**, which is the
//! other half of that quote: *"The intended key is the operator's existing sshd
//! `authorized_keys` identity — no new secret to manage."* [`Account::from_ssh`]
//! parses exactly that, so `root` stops being a string anyone can type and
//! becomes possession of the key already used to reach the machine.

use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

/// A public key, and therefore an account. 32 bytes, no indirection.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Account(pub [u8; 32]);

#[derive(Debug, PartialEq, Eq)]
pub enum KeyError {
    /// Not an `ssh-ed25519` line. RSA and ECDSA keys are not accepted — this
    /// is an ed25519 chain and converting would be inventing an identity.
    NotEd25519,
    Malformed(&'static str),
    BadSignature,
}

impl core::fmt::Display for KeyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            KeyError::NotEd25519 => write!(f, "not an ssh-ed25519 key"),
            KeyError::Malformed(w) => write!(f, "malformed ssh key: {w}"),
            KeyError::BadSignature => write!(f, "signature does not verify"),
        }
    }
}

impl std::error::Error for KeyError {}

impl Account {
    /// Parse one `authorized_keys` line.
    ///
    /// The wire format inside the base64 is a sequence of length-prefixed
    /// strings: the algorithm name, then the key. Both lengths are big-endian
    /// `u32`, and every one of them is checked — a truncated key must be a
    /// parse error, never a silently short account.
    pub fn from_ssh(line: &str) -> Result<Self, KeyError> {
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
        let key: [u8; 32] =
            raw[c..d].try_into().map_err(|_| KeyError::Malformed("key is not 32 bytes"))?;
        Ok(Account(key))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Short form for logs: the first four bytes, like a git hash.
    pub fn short(&self) -> String {
        self.0[..4].iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn verify(&self, payload: &[u8], sig: &[u8; 64]) -> Result<(), KeyError> {
        let vk = VerifyingKey::from_bytes(&self.0).map_err(|_| KeyError::BadSignature)?;
        vk.verify(payload, &Signature::from_bytes(sig)).map_err(|_| KeyError::BadSignature)
    }
}

impl core::fmt::Debug for Account {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Account({})", self.short())
    }
}

impl core::fmt::Display for Account {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.short())
    }
}

/// A cat's own key. The secret never leaves the process that made it.
pub struct Identity {
    key: SigningKey,
}

impl Identity {
    /// A fresh identity from the OS entropy source.
    pub fn generate() -> Self {
        Identity { key: SigningKey::generate(&mut rand_core::OsRng) }
    }

    /// From a 32-byte seed — reproducible, for tests and for a litter whose
    /// membership has to be the same on every run.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Identity { key: SigningKey::from_bytes(seed) }
    }

    pub fn account(&self) -> Account {
        Account(self.key.verifying_key().to_bytes())
    }

    pub fn sign(&self, payload: &[u8]) -> [u8; 64] {
        self.key.sign(payload).to_bytes()
    }
}

/// One act, with its signature.
///
/// `who` is **not** trusted: it is the key the signature is checked against,
/// and [`SignedCall::verify`] is the only way to get an [`Account`] out. A
/// caller cannot obtain the sender without the signature having verified, which
/// is what makes a forged sender impossible rather than merely disallowed.
pub struct SignedCall {
    who: Account,
    payload: Vec<u8>,
    sig: [u8; 64],
}

impl SignedCall {
    /// Sign `payload`. The payload is whatever the caller canonicalised —
    /// typically the encoded call plus a nonce and the genesis id, so a
    /// signature cannot be replayed onto another chain or another height.
    pub fn new(id: &Identity, payload: Vec<u8>) -> Self {
        SignedCall { who: id.account(), sig: id.sign(&payload), payload }
    }

    pub fn from_parts(who: Account, payload: Vec<u8>, sig: [u8; 64]) -> Self {
        SignedCall { who, payload, sig }
    }

    /// The sender, **recovered** — available only if the signature verifies.
    pub fn verify(&self) -> Result<Account, KeyError> {
        self.who.verify(&self.payload, &self.sig)?;
        Ok(self.who)
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    pub fn signature(&self) -> &[u8; 64] {
        &self.sig
    }

    /// The claimed sender, unverified. Named so that using it without
    /// [`SignedCall::verify`] reads as the mistake it is.
    pub fn claimed_account(&self) -> Account {
        self.who
    }
}

#[cfg(test)]
mod tests;
