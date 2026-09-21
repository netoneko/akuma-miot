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

    /// This identity as an `authorized_keys` line.
    ///
    /// The inverse of [`Account::from_ssh`], which is what lets the parser be
    /// tested on keys generated here rather than on somebody's real one —
    /// a repository is the wrong place to keep a person's identity, public or
    /// not.
    pub fn ssh_public_line(&self, comment: &str) -> String {
        let key = self.account().0;
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

/// What a signature is *over*.
///
/// Never the bare call. Four fields, and each one closes a specific hole:
///
/// - `DOMAIN` — a context string, so bytes signed here can never verify as
///   some other message type this project signs later. Cheap now, impossible
///   to retrofit once keys are in use.
/// - `genesis` — the chain this call is for. Without it a signature from a
///   test chain is valid on the real one, and a cat that ever touched a dev
///   chain has handed over replayable authority.
/// - `nonce` — per-account, monotonic. Without it, `clear t1.1` signed once is
///   replayable by anyone who saw it, forever.
/// - `call` — the SCALE-encoded call itself.
///
/// Both sides build the bytes through [`Envelope::signing_bytes`] and nowhere
/// else. A second encoder is how signer and verifier drift into intermittent
/// failures, and length-prefixing every variable field is how two different
/// calls avoid encoding to the same bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
    pub genesis: [u8; 32],
    pub nonce: u64,
    pub call: Vec<u8>,
}

/// Context string. Changing it invalidates every signature ever made.
const DOMAIN: &[u8] = b"akuma-miot/v1/call";

impl Envelope {
    pub fn new(genesis: [u8; 32], nonce: u64, call: Vec<u8>) -> Self {
        Envelope { genesis, nonce, call }
    }

    /// The exact bytes that get signed. **The only place they are built.**
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(DOMAIN.len() + 48 + self.call.len());
        // Length-prefixed so a domain and a call can never be concatenated
        // into each other — the classic way two distinct messages end up with
        // one signature.
        v.extend_from_slice(&(DOMAIN.len() as u32).to_be_bytes());
        v.extend_from_slice(DOMAIN);
        v.extend_from_slice(&self.genesis);
        v.extend_from_slice(&self.nonce.to_be_bytes());
        v.extend_from_slice(&(self.call.len() as u32).to_be_bytes());
        v.extend_from_slice(&self.call);
        v
    }
}

/// Why an envelope was rejected. Separate from [`KeyError`] so a caller can
/// tell "this is not who it says" from "this is stale".
#[derive(Debug, PartialEq, Eq)]
pub enum CallError {
    /// The signature does not verify for the claimed key.
    BadSignature,
    /// Signed for a different chain.
    WrongChain,
    /// Already used, or out of order.
    BadNonce { expected: u64, got: u64 },
}

impl core::fmt::Display for CallError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CallError::BadSignature => write!(f, "signature does not verify"),
            CallError::WrongChain => write!(f, "signed for a different chain"),
            CallError::BadNonce { expected, got } => {
                write!(f, "nonce {got} is not {expected}")
            }
        }
    }
}

impl std::error::Error for CallError {}

/// A call, its envelope, and a signature over both.
pub struct SignedEnvelope {
    who: Account,
    env: Envelope,
    sig: [u8; 64],
}

impl SignedEnvelope {
    pub fn new(id: &Identity, env: Envelope) -> Self {
        let sig = id.sign(&env.signing_bytes());
        SignedEnvelope { who: id.account(), env, sig }
    }

    pub fn from_parts(who: Account, env: Envelope, sig: [u8; 64]) -> Self {
        SignedEnvelope { who, env, sig }
    }

    pub fn envelope(&self) -> &Envelope {
        &self.env
    }

    pub fn signature(&self) -> &[u8; 64] {
        &self.sig
    }

    /// The claimed sender, unverified. Named so that using it without
    /// [`SignedEnvelope::check`] reads as the mistake it is.
    pub fn claimed_account(&self) -> Account {
        self.who
    }

    /// Recover the sender, or say why not.
    ///
    /// Order matters: the **signature is checked first**. Reporting
    /// `WrongChain` or `BadNonce` for bytes nobody proved they authored would
    /// be answering questions about a message that does not exist.
    pub fn check(&self, genesis: &[u8; 32], expected_nonce: u64) -> Result<Account, CallError> {
        self.who
            .verify(&self.env.signing_bytes(), &self.sig)
            .map_err(|_| CallError::BadSignature)?;
        if &self.env.genesis != genesis {
            return Err(CallError::WrongChain);
        }
        if self.env.nonce != expected_nonce {
            return Err(CallError::BadNonce {
                expected: expected_nonce,
                got: self.env.nonce,
            });
        }
        Ok(self.who)
    }

    /// The call bytes — available **only** once `check` has passed.
    pub fn into_call(self, genesis: &[u8; 32], nonce: u64) -> Result<(Account, Vec<u8>), CallError> {
        let who = self.check(genesis, nonce)?;
        Ok((who, self.env.call))
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
