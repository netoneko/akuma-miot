use super::*;

/// The operator's real key, from `~/.akuma/ssh/id_ed25519.pub`.
const OPERATOR: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGADiMSbnANnXaUnxpgWBJKF2KfqmTyK80f0ApDVP4Vy netoneko@localhost";

#[test]
fn the_operators_ssh_key_parses_to_an_account() {
    let root = Account::from_ssh(OPERATOR).unwrap();
    assert_eq!(root.as_bytes().len(), 32);
    // The first bytes are visible in the base64 payload after the header, so
    // this pins that we parsed the key field and not some adjacent bytes.
    assert_eq!(root.short(), "6003_8c".replace('_', ""));
}

#[test]
fn the_comment_and_extra_whitespace_do_not_matter() {
    let a = Account::from_ssh(OPERATOR).unwrap();
    let b = Account::from_ssh(&format!("  {}  ", OPERATOR.rsplit_once(' ').unwrap().0)).unwrap();
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
    let good = Account::from_ssh(OPERATOR).unwrap();
    let b64 = OPERATOR.split_whitespace().nth(1).unwrap();
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
    let root = Account::from_ssh(OPERATOR).unwrap();
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
