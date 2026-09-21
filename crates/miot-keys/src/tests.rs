use super::*;
use sp_core::Pair as _;

/// A throwaway operator identity, derived from a fixed seed at test time.
/// Deliberately **not** anybody's real key.
fn operator() -> Identity {
    Identity::from_seed(&[0xA1; 32])
}

fn operator_line() -> String {
    operator().ssh_public_line("operator@example")
}

/// The writer and the parser are independent code paths — one builds the
/// length-prefixed wire format, the other walks it — so agreement is evidence
/// rather than tautology.
#[test]
fn an_ssh_key_line_parses_back_to_its_account() {
    let id = operator();
    let parsed = account_from_ssh(&id.ssh_public_line("operator@example")).unwrap();
    assert_eq!(parsed, id.account());
}

/// For ed25519 the account **is** the public key, which is the whole reason a
/// sender can be recovered from a signature rather than asserted beside one.
#[test]
fn an_account_is_the_public_key_itself_not_a_hash_of_it() {
    let id = operator();
    assert_eq!(AsRef::<[u8]>::as_ref(&id.account()), &id.public().0[..]);
}

#[test]
fn the_wire_layout_is_two_length_prefixed_fields() {
    let id = Identity::from_seed(&[9u8; 32]);
    let line = id.ssh_public_line("x@y");
    let b64 = line.split_whitespace().nth(1).unwrap();
    let raw = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();

    assert_eq!(u32::from_be_bytes(raw[0..4].try_into().unwrap()), 11);
    assert_eq!(&raw[4..15], b"ssh-ed25519");
    assert_eq!(u32::from_be_bytes(raw[15..19].try_into().unwrap()), 32);
    assert_eq!(&raw[19..51], &id.public().0[..]);
    assert_eq!(raw.len(), 51, "nothing trailing");
}

#[test]
fn the_comment_and_extra_whitespace_do_not_matter() {
    let line = operator_line();
    let a = account_from_ssh(&line).unwrap();
    let b = account_from_ssh(&format!("  {}  ", line.rsplit_once(' ').unwrap().0)).unwrap();
    assert_eq!(a, b);
}

#[test]
fn only_ed25519_keys_are_accepted() {
    let rsa = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQC7 user@host";
    assert_eq!(public_from_ssh(rsa).unwrap_err(), KeyError::NotEd25519);
}

#[test]
fn a_truncated_key_is_a_parse_error_not_a_short_account() {
    let line = operator_line();
    let b64 = line.split_whitespace().nth(1).unwrap();
    let cut = &b64[..b64.len() - 8];
    let r = public_from_ssh(&format!("ssh-ed25519 {cut} user@host"));
    assert!(matches!(r, Err(KeyError::Malformed(_))), "got {r:?}");
}

#[test]
fn garbage_does_not_parse() {
    assert!(public_from_ssh("").is_err());
    assert!(public_from_ssh("ssh-ed25519").is_err());
    assert!(public_from_ssh("ssh-ed25519 AAAAnotbase64!!!").is_err());
}

#[test]
fn a_seeded_identity_is_reproducible() {
    assert_eq!(Identity::from_seed(&[7u8; 32]).account(), Identity::from_seed(&[7u8; 32]).account());
    assert_ne!(Identity::from_seed(&[7u8; 32]).account(), Identity::from_seed(&[8u8; 32]).account());
}

/// Verification is `sp_core`'s, not ours. This pins that an account recovered
/// from an SSH line is the one a signature verifies against — the join between
/// the half we own and the half polkadot-sdk owns.
#[test]
fn a_signature_verifies_against_the_account_parsed_from_ssh() {
    let id = operator();
    let from_ssh = public_from_ssh(&operator_line()).unwrap();
    let msg = b"TaskUpdate{t1.1,done}";
    let sig = id.sign(msg);

    assert!(sp_core::ed25519::Pair::verify(&sig, msg, &from_ssh));
    assert!(!sp_core::ed25519::Pair::verify(&sig, b"TaskUpdate{t1.2,done}", &from_ssh));

    let other = Identity::from_seed(&[0xB2; 32]);
    assert!(!sp_core::ed25519::Pair::verify(&sig, msg, &other.public()));
}

/// Root stops being a string anyone can type and becomes possession of a key.
#[test]
fn root_authority_is_possession_of_the_operator_key() {
    let root = account_from_ssh(&operator_line()).unwrap();
    let impostor = Identity::from_seed(&[0xC3; 32]);
    assert_ne!(impostor.account(), root, "a cat can say what it likes; it cannot BE the operator");
}

#[test]
fn short_form_is_the_leading_bytes() {
    let id = Identity::from_seed(&[0u8; 32]);
    let s = short(&id.account());
    assert_eq!(s.len(), 8);
    assert_eq!(s, hex::encode(&id.public().0[..4]));
}
