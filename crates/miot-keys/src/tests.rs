use super::*;

/// A throwaway operator identity, derived from a fixed seed at test time.
///
/// Deliberately **not** anybody's real key. An operator's public key belongs in
/// their own `~/.akuma/ssh/id_ed25519.pub` and is read from there at startup;
/// a repository is the wrong place to keep a person's identity, public or not.
fn operator() -> Identity {
    Identity::from_seed(&[0xA1; 32])
}

fn operator_line() -> String {
    operator().ssh_public_line("operator@example")
}

/// An `authorized_keys` line round-trips to the account that produced it.
///
/// The writer and the parser are independent code paths — one builds the
/// length-prefixed wire format, the other walks it — so agreement between them
/// is evidence rather than tautology.
#[test]
fn an_ssh_key_line_parses_back_to_its_account() {
    let id = operator();
    let parsed = Account::from_ssh(&id.ssh_public_line("operator@example")).unwrap();
    assert_eq!(parsed, id.account());
    assert_eq!(parsed.as_bytes().len(), 32);
}

/// Pins the on-the-wire layout against a hand-built line: the header is
/// `ssh-ed25519` at offset 4, and the key is the 32 bytes after its own
/// big-endian length. A change in either field's framing fails here.
#[test]
fn the_wire_layout_is_two_length_prefixed_fields() {
    let id = Identity::from_seed(&[9u8; 32]);
    let line = id.ssh_public_line("x@y");
    let b64 = line.split_whitespace().nth(1).unwrap();
    let raw = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();

    assert_eq!(u32::from_be_bytes(raw[0..4].try_into().unwrap()), 11);
    assert_eq!(&raw[4..15], b"ssh-ed25519");
    assert_eq!(u32::from_be_bytes(raw[15..19].try_into().unwrap()), 32);
    assert_eq!(&raw[19..51], id.account().as_bytes());
    assert_eq!(raw.len(), 51, "nothing trailing");
}

#[test]
fn the_comment_and_extra_whitespace_do_not_matter() {
    let line = operator_line();
    let a = Account::from_ssh(&line).unwrap();
    let b = Account::from_ssh(&format!("  {}  ", line.rsplit_once(' ').unwrap().0)).unwrap();
    assert_eq!(a, b);
}

#[test]
fn only_ed25519_keys_are_accepted() {
    // A well-formed ssh-rsa header. Converting it would be inventing an
    // identity, so it is refused rather than coerced.
    let rsa = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQC7 user@host";
    assert_eq!(Account::from_ssh(rsa).unwrap_err(), KeyError::NotEd25519);
}

#[test]
fn a_truncated_key_is_a_parse_error_not_a_short_account() {
    let line = operator_line();
    let good = Account::from_ssh(&line).unwrap();
    let b64 = line.split_whitespace().nth(1).unwrap();
    // Lop off the tail of the base64: the length prefix will now overrun.
    let cut = &b64[..b64.len() - 8];
    let line = format!("ssh-ed25519 {cut} user@host");
    let r = Account::from_ssh(&line);
    assert!(
        matches!(r, Err(KeyError::Malformed(_))),
        "a truncated key must not parse; got {r:?} (good was {good})"
    );
}

#[test]
fn garbage_does_not_parse() {
    assert!(Account::from_ssh("").is_err());
    assert!(Account::from_ssh("ssh-ed25519").is_err());
    assert!(Account::from_ssh("ssh-ed25519 AAAAnotbase64!!!").is_err());
}

// ---- signing --------------------------------------------------------------

#[test]
fn a_seeded_identity_is_reproducible() {
    let a = Identity::from_seed(&[7u8; 32]);
    let b = Identity::from_seed(&[7u8; 32]);
    assert_eq!(a.account(), b.account());
    assert_ne!(a.account(), Identity::from_seed(&[8u8; 32]).account());
}

#[test]
fn generated_identities_differ() {
    assert_ne!(Identity::generate().account(), Identity::generate().account());
}

#[test]
fn a_signature_verifies_against_its_own_account_and_no_other() {
    let tama = Identity::generate();
    let kuro = Identity::generate();
    let payload = b"TaskUpdate{t1.1,done}";
    let sig = tama.sign(payload);

    assert!(tama.account().verify(payload, &sig).is_ok());
    assert_eq!(kuro.account().verify(payload, &sig).unwrap_err(), KeyError::BadSignature);
}

#[test]
fn changing_one_byte_of_the_payload_breaks_it() {
    let id = Identity::generate();
    let sig = id.sign(b"clear t1.1");
    assert_eq!(id.account().verify(b"clear t1.2", &sig).unwrap_err(), KeyError::BadSignature);
}

// ---- the prize: the sender is recovered, never claimed ---------------------

#[test]
fn a_signed_call_yields_its_sender() {
    let tama = Identity::generate();
    let call = SignedCall::new(&tama, b"done t1.1".to_vec());
    assert_eq!(call.verify().unwrap(), tama.account());
}

/// *"A forged `from` is not a policy failure, it is a signature that does not
/// recover to anyone in the roster."*
#[test]
fn a_forged_sender_is_not_a_policy_failure_but_an_invalid_signature() {
    let kuro = Identity::generate();
    let mimi = Identity::generate();

    // kuro signs, then swaps the sender to mimi to claim leader authority.
    let honest = SignedCall::new(&kuro, b"clear t1.1".to_vec());
    let forged = SignedCall::from_parts(
        mimi.account(),
        honest.payload().to_vec(),
        *honest.signature(),
    );

    // The claim is there for the taking — and buys nothing, because the only
    // way to obtain an Account is through verify().
    assert_eq!(forged.claimed_account(), mimi.account());
    assert_eq!(forged.verify().unwrap_err(), KeyError::BadSignature);
}

/// Root stops being a string anyone can type and becomes possession of a key.
#[test]
fn root_authority_is_possession_of_the_operator_key() {
    let root = Account::from_ssh(&operator_line()).unwrap();
    let impostor = Identity::generate();

    let call = SignedCall::new(&impostor, b"open t1".to_vec());
    let sender = call.verify().expect("the impostor signed honestly, so it verifies");

    // It verifies — and it is simply not root.
    assert_ne!(sender, root);
    assert_eq!(
        impostor.account(),
        sender,
        "an unprivileged cat can say what it likes; it cannot BE the operator"
    );
}

// ---- the envelope: one test per way this goes wrong -----------------------

const CHAIN: [u8; 32] = [0x11; 32];
const OTHER_CHAIN: [u8; 32] = [0x22; 32];

fn call(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

fn signed(id: &Identity, nonce: u64, c: &str) -> SignedEnvelope {
    SignedEnvelope::new(id, Envelope::new(CHAIN, nonce, call(c)))
}

#[test]
fn an_honest_call_recovers_its_sender() {
    let tama = Identity::generate();
    let env = signed(&tama, 0, "clear t1.1");
    assert_eq!(env.check(&CHAIN, 0).unwrap(), tama.account());
}

/// Without a nonce, `clear t1.1` signed once is replayable by anyone who saw
/// it, forever.
#[test]
fn a_replayed_call_is_refused_by_its_nonce() {
    let tama = Identity::generate();
    let env = signed(&tama, 7, "done t1.1");

    assert!(env.check(&CHAIN, 7).is_ok(), "accepted once");
    // The same bytes, offered again after the account moved on.
    assert_eq!(
        env.check(&CHAIN, 8).unwrap_err(),
        CallError::BadNonce { expected: 8, got: 7 },
        "and never again"
    );
}

/// A signature from a dev chain must not be authority on a real one.
#[test]
fn a_call_signed_for_another_chain_is_refused() {
    let tama = Identity::generate();
    let env = signed(&tama, 0, "open t1");
    assert_eq!(env.check(&OTHER_CHAIN, 0).unwrap_err(), CallError::WrongChain);
}

/// The signature is checked before anything else: answering "wrong chain" for
/// bytes nobody proved they authored would be discussing a message that does
/// not exist.
#[test]
fn a_forgery_is_reported_as_a_forgery_not_as_a_stale_nonce() {
    let kuro = Identity::generate();
    let mimi = Identity::generate();
    let honest = signed(&kuro, 3, "clear t1.1");
    let forged = SignedEnvelope::from_parts(
        mimi.account(),
        honest.envelope().clone(),
        *honest.signature(),
    );
    // Wrong chain AND wrong nonce AND a forged sender — it is the forgery that
    // is reported.
    assert_eq!(
        forged.check(&OTHER_CHAIN, 99).unwrap_err(),
        CallError::BadSignature
    );
}

#[test]
fn tampering_with_any_field_breaks_the_signature() {
    let tama = Identity::generate();
    let orig = signed(&tama, 5, "clear t1.1");

    for tampered in [
        Envelope::new(CHAIN, 5, call("clear t1.2")), // the call
        Envelope::new(CHAIN, 6, call("clear t1.1")), // the nonce
        Envelope::new(OTHER_CHAIN, 5, call("clear t1.1")), // the chain
    ] {
        let e = SignedEnvelope::from_parts(tama.account(), tampered, *orig.signature());
        assert_eq!(
            e.check(&CHAIN, 5).unwrap_err(),
            CallError::BadSignature,
            "every field is covered by the signature"
        );
    }
}

/// Length-prefixing is what stops two different envelopes producing identical
/// signing bytes by sliding a boundary.
#[test]
fn field_boundaries_cannot_be_slid() {
    let a = Envelope::new(CHAIN, 0, call("ab"));
    let b = Envelope::new(CHAIN, 0, call("a"));
    assert_ne!(a.signing_bytes(), b.signing_bytes());

    // A call that starts with what looks like a length prefix must not be
    // reinterpretable as a shorter call plus trailing data.
    let c = Envelope::new(CHAIN, 0, call("\u{0}\u{0}\u{0}\u{1}x"));
    let d = Envelope::new(CHAIN, 0, call("x"));
    assert_ne!(c.signing_bytes(), d.signing_bytes());
}

/// The domain string is in the signed bytes, so a signature made here can
/// never verify as some other kind of message this project signs later.
#[test]
fn the_domain_is_covered_by_the_signature() {
    let env = Envelope::new(CHAIN, 0, call("x"));
    let bytes = env.signing_bytes();
    assert!(
        bytes.windows(DOMAIN.len()).any(|w| w == DOMAIN),
        "the context string is part of what gets signed"
    );
    assert_eq!(&bytes[0..4], &(DOMAIN.len() as u32).to_be_bytes());
}

/// The call bytes are only reachable through a successful check.
#[test]
fn the_call_is_unavailable_until_the_signature_passes() {
    let kuro = Identity::generate();
    let mimi = Identity::generate();
    let honest = signed(&kuro, 0, "artifact t1");
    let forged =
        SignedEnvelope::from_parts(mimi.account(), honest.envelope().clone(), *honest.signature());

    assert!(forged.into_call(&CHAIN, 0).is_err());
    let (who, c) = signed(&kuro, 0, "artifact t1").into_call(&CHAIN, 0).unwrap();
    assert_eq!(who, kuro.account());
    assert_eq!(c, call("artifact t1"));
}

/// Two cats at the same nonce is normal — nonces are per account, not global.
#[test]
fn nonces_are_per_account() {
    let tama = Identity::generate();
    let kuro = Identity::generate();
    assert!(signed(&tama, 0, "claim t1.1").check(&CHAIN, 0).is_ok());
    assert!(signed(&kuro, 0, "claim t1.2").check(&CHAIN, 0).is_ok());
}

/// After a rewind a cat resubmits work the chain discarded. That must still
/// verify — the signature is over the call, not over a block — and it is the
/// nonce, not the signature, that decides whether it is fresh.
#[test]
fn a_resubmission_after_a_rewind_still_verifies() {
    let tama = Identity::generate();
    let env = signed(&tama, 4, "done t1.1");
    assert!(env.check(&CHAIN, 4).is_ok());
    // The block holding it was discarded; the account's nonce went back to 4.
    assert!(
        signed(&tama, 4, "done t1.1").check(&CHAIN, 4).is_ok(),
        "losing a record must not cost a cat its ability to say the thing again"
    );
}
