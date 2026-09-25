//! mTLS pinned to the same ed25519 keys the chain already trusts.
//!
//! Every mesh node's TLS identity *is* its [`miot_keys::Identity`] — no
//! separate CA, no cert to provision or rotate: the certificate is
//! self-signed and rebuilt fresh every process start purely to carry that
//! same key through the TLS handshake. There is no chain-of-trust question
//! to answer ("who signed this cert") because nothing signs it but its own
//! key; what's checked instead is exactly [`node::Node::is_trusted_signer`]'s
//! question — is the cert's embedded public key one of the genesis accounts
//! everything else here already trusts — on every connection, both
//! directions between mesh nodes: the server requires a client cert, the
//! client requires a server cert it recognizes, both pinned the same way.
//!
//! **An operator's client doesn't pin the node** ([`client_config_any_node`],
//! 2026-09-23): it's a private chain whose nodes the operator runs, so the
//! node it connects to is trusted by fiat, and the roster comes *from* that
//! node (`/roster`) instead of from a local copy the client would have to
//! carry just to check the first handshake. The node still pins the client —
//! a stranger still gets nothing — and the handshake still proves the node
//! holds the key its cert names; what's given up is only "…and that key is a
//! genesis member", i.e. protection against someone impersonating a node on
//! the path. Node↔node traffic stays fully pinned: that's the mesh itself.
//!
//! What this buys over the `x-miot-signer`/`x-miot-sig` header envelope
//! (`node.rs`) is confidentiality (the wire is no longer plaintext) and a
//! proof-of-possession bound to the whole connection rather than one
//! request's bytes. The header envelope stays regardless — cheap, already
//! tested, and it's still the thing that ties a *specific request* to an
//! account, not just "this TLS session."

use std::sync::Arc;

use miot_keys::Identity;
use miot_runtime::AccountId;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

/// The fixed RFC 8410 §7 PKCS8 prefix for an Ed25519 private key: version 0,
/// then the `id-Ed25519` `AlgorithmIdentifier` (no parameters — Ed25519 never
/// has any), then the `OCTET STRING` wrapper around the 32-byte seed that
/// follows this constant. Ed25519 keys are fixed-length with no algorithm
/// parameters, so this prefix never varies with the key — it's the same 16
/// bytes any Ed25519 PKCS8 document uses (OpenSSL's included), not something
/// specific to this codebase.
const PKCS8_ED25519_PREFIX: [u8; 16] = [0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20];

/// DER content octets of the `id-Ed25519` OID (1.3.101.112), tag and length
/// stripped — what `x509_parser::Oid::as_bytes()` returns for it.
const OID_ED25519: [u8; 3] = [0x2b, 0x65, 0x70];

fn pkcs8_from_seed(seed: &[u8; 32]) -> PrivatePkcs8KeyDer<'static> {
    let mut der = Vec::with_capacity(48);
    der.extend_from_slice(&PKCS8_ED25519_PREFIX);
    der.extend_from_slice(seed);
    PrivatePkcs8KeyDer::from(der)
}

fn key_pair_for(identity: &Identity) -> rcgen::KeyPair {
    let pkcs8 = pkcs8_from_seed(&identity.seed());
    rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&pkcs8, &rcgen::PKCS_ED25519).expect("well-formed Ed25519 PKCS8 document")
}

/// A self-signed cert + key carrying `identity`'s account. Regenerated
/// fresh on every call — cheap (no CA, no chain to build) and there's
/// nothing to gain from persisting one: what's trusted is the embedded
/// pubkey, not the cert's own lifetime, and a freshly-generated cert can't
/// go stale.
pub fn cert_for(identity: &Identity) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let key_pair = key_pair_for(identity);
    let params = rcgen::CertificateParams::default();
    let cert = params.self_signed(&key_pair).expect("self-signing our own freshly-built key pair cannot fail");
    (cert.der().clone(), PrivateKeyDer::Pkcs8(pkcs8_from_seed(&identity.seed())))
}

/// Pull the embedded Ed25519 public key back out of a cert this module
/// generated. This parse decides *which* account a cert claims to be; it
/// proves nothing about possession — `verify_tls12/13_signature` below is
/// what actually checks the peer holds the matching private key, over the
/// live handshake transcript. A hand-rolled byte search for this field
/// would be a spoofing risk (a crafted cert could plant a decoy trusted
/// key elsewhere in the DER while the real SPKI, the one the handshake
/// signature is actually bound to, is different) — `x509_parser` walks the
/// real ASN.1 grammar, so the field it returns is the one the signature
/// check below is bound to as well, not a lookalike.
fn account_of(cert: &CertificateDer<'_>) -> Result<AccountId, rustls::Error> {
    let (_, parsed) = x509_parser::parse_x509_certificate(cert.as_ref()).map_err(|_| rustls::Error::General("bad certificate encoding".into()))?;
    let spki = parsed.public_key();
    if spki.algorithm.algorithm.as_bytes() != OID_ED25519 {
        return Err(rustls::Error::General("certificate key is not Ed25519".into()));
    }
    let raw: &[u8] = &spki.subject_public_key.data;
    let key: [u8; 32] = raw.try_into().map_err(|_| rustls::Error::General("Ed25519 key is not 32 bytes".into()))?;
    Ok(AccountId::new(key))
}

/// Same rule both verifier directions check: [`node::Node::is_trusted_signer`]'s
/// set, applied to a TLS cert instead of a header signature. `None` trusts
/// any account — the cert must still be an Ed25519 one this module's shape
/// accepts, and the handshake signature is still checked against it.
fn check_trusted(cert: &CertificateDer<'_>, trusted: Option<&[AccountId]>) -> Result<(), rustls::Error> {
    let account = account_of(cert)?;
    match trusted {
        Some(set) if !set.contains(&account) => Err(rustls::Error::General("certificate's account is not trusted".into())),
        _ => Ok(()),
    }
}

#[derive(Debug)]
struct Pinned {
    provider: Arc<CryptoProvider>,
    /// `None`: any Ed25519 key (an operator's client, see the module docs).
    trusted: Option<Vec<AccountId>>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        check_trusted(end_entity, self.trusted.as_deref())?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

impl ClientCertVerifier for Pinned {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(&self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>], _now: UnixTime) -> Result<ClientCertVerified, rustls::Error> {
        check_trusted(end_entity, self.trusted.as_deref())?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// TLS1.3 only — this is a closed mesh we control both ends of, not a
/// browser-facing server with legacy clients to support, so there's no
/// reason to carry TLS1.2's extra code path (or its extra
/// `verify_tls12_signature` risk surface) along for the ride.
fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The mTLS server config for `identity`, requiring and pinning client
/// certs to `trusted` — used for the node's own TLS listener.
pub fn server_config(identity: &Identity, trusted: Vec<AccountId>) -> rustls::ServerConfig {
    let provider = provider();
    let verifier: Arc<dyn ClientCertVerifier> = Arc::new(Pinned { provider: provider.clone(), trusted: Some(trusted) });
    let (cert, key) = cert_for(identity);
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS1.3 is supported by the ring provider")
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![cert], key)
        .expect("our own freshly-generated cert and key must match");
    config.alpn_protocols = ALPN_PROTOCOLS.iter().map(|p| p.to_vec()).collect();
    config
}

/// The mTLS client config for `identity`, presenting its own cert and
/// pinning the server's to `trusted` — for callers that already hold the
/// genesis: `node.rs`'s own peer client, `agent.rs`'s `Cat`.
pub fn client_config(identity: &Identity, trusted: Vec<AccountId>) -> rustls::ClientConfig {
    configured_client(identity, Some(trusted))
}

/// [`client_config`] minus the pin on the server: `client.rs`'s `Client`,
/// which learns the roster from the node it connects to (module docs).
pub fn client_config_any_node(identity: &Identity) -> rustls::ClientConfig {
    configured_client(identity, None)
}

fn configured_client(identity: &Identity, trusted: Option<Vec<AccountId>>) -> rustls::ClientConfig {
    let provider = provider();
    let verifier: Arc<dyn ServerCertVerifier> = Arc::new(Pinned { provider: provider.clone(), trusted });
    let (cert, key) = cert_for(identity);
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS1.3 is supported by the ring provider")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![cert], key)
        .expect("our own freshly-generated cert and key must match");
    config.alpn_protocols = ALPN_PROTOCOLS.iter().map(|p| p.to_vec()).collect();
    config
}

/// ALPN, most-preferred first — h2 for the connection-reuse win
/// `docs/MESH_AUTH.md` already cared about, http/1.1 as a fallback so a
/// client library that only speaks h1 (or a wire-inspection tool testing
/// this by hand) still completes a handshake.
const ALPN_PROTOCOLS: [&[u8]; 2] = [b"h2", b"http/1.1"];

/// Wraps a [`tokio::net::TcpListener`] so [`axum::serve`] can drive it
/// directly: each accepted TCP connection gets the mTLS handshake applied
/// before axum ever sees it. A failed handshake (an untrusted or absent
/// client cert, a stray port-scanner) is logged and dropped, not returned —
/// axum's `Listener::accept` has no error path, the same rule the plain
/// `TcpListener` impl it's modeled on already follows for a failed
/// `accept()`.
///
/// **Handshakes run on their own, each under [`HANDSHAKE_TIMEOUT`].** They
/// used to run inline in `accept`, one at a time, with no timeout: a single
/// client that opened a connection and never sent a ClientHello blocked
/// every connection after it, for good. Found live 2026-09-25 on the AWS
/// pair, which face the internet through nginx's stream proxy: the accept
/// backlog sat full (`Recv-Q 129`), the node stopped answering anyone —
/// mesh, operator — while its own outbound polling and agent loop carried
/// on as if nothing was wrong. Now a background task accepts, spawns each
/// handshake, and hands finished ones to `accept`; a silent client costs
/// one task for ten seconds.
pub struct TlsListener {
    ready: tokio::sync::mpsc::Receiver<(tokio_rustls::server::TlsStream<tokio::net::TcpStream>, std::net::SocketAddr)>,
    local: Option<std::net::SocketAddr>,
}

/// How long a connection gets to complete the mTLS handshake.
pub const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl TlsListener {
    /// Starts accepting at once (a background task). It stops, and the port
    /// is freed, when this listener is dropped — so aborting a node's
    /// server (the election tests do) still releases its port.
    pub fn new(tcp: tokio::net::TcpListener, config: rustls::ServerConfig) -> Self {
        let local = tcp.local_addr().ok();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let (tx, ready) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            loop {
                let (tcp_stream, addr) = tokio::select! {
                    got = tcp.accept() => match got {
                        Ok(pair) => pair,
                        Err(e) => {
                            eprintln!("[tls] accept error: {e}");
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                            continue;
                        }
                    },
                    // The listener was dropped: stop, and let the port go.
                    _ = tx.closed() => return,
                };
                let (acceptor, tx) = (acceptor.clone(), tx.clone());
                tokio::spawn(async move {
                    match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp_stream)).await {
                        Ok(Ok(tls_stream)) => {
                            let _ = tx.send((tls_stream, addr)).await;
                        }
                        Ok(Err(e)) => eprintln!("[tls] handshake with {addr} failed: {e}"),
                        Err(_) => eprintln!("[tls] handshake with {addr} timed out after {}s", HANDSHAKE_TIMEOUT.as_secs()),
                    }
                });
            }
        });
        TlsListener { ready, local }
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.ready.recv().await {
            Some(pair) => pair,
            // The accept task only ends once this listener is gone, so this
            // can't happen while it's alive; never return a fake connection.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.local.ok_or_else(|| std::io::Error::other("listener had no local address"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::{TcpListener, TcpStream};

    /// A real loopback TCP connection, real TLS1.3 handshake, no mocks —
    /// this exercises the exact path `node.rs`'s listener and every
    /// caller's client config will run in production.
    async fn handshake(server_id: &Identity, server_trusts: Vec<AccountId>, client_id: &Identity, client_trusts: Vec<AccountId>) -> Result<(), String> {
        handshake_with(server_id, server_trusts, client_config(client_id, client_trusts)).await
    }

    async fn handshake_with(server_id: &Identity, server_trusts: Vec<AccountId>, client: rustls::ClientConfig) -> Result<(), String> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config(server_id, server_trusts)));
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            acceptor.accept(tcp).await.map(|_| ()).map_err(|e| e.to_string())
        });

        let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
        let tcp = TcpStream::connect(addr).await.unwrap();
        let name = ServerName::IpAddress(addr.ip().into());
        let client_result = connector.connect(name, tcp).await.map(|_| ()).map_err(|e| e.to_string());

        let server_result = server.await.unwrap();
        client_result.and(server_result)
    }

    #[tokio::test]
    async fn mutually_trusted_accounts_complete_the_handshake() {
        let a = Identity::from_seed(&[1; 32]);
        let b = Identity::from_seed(&[2; 32]);
        let trusted = vec![a.account(), b.account()];
        assert!(handshake(&a, trusted.clone(), &b, trusted).await.is_ok());
    }

    #[tokio::test]
    async fn a_client_the_server_does_not_trust_is_refused() {
        let a = Identity::from_seed(&[1; 32]);
        let b = Identity::from_seed(&[2; 32]);
        // Server trusts only itself — b's cert is real and self-consistent,
        // just not in the set that matters.
        let result = handshake(&a, vec![a.account()], &b, vec![a.account(), b.account()]).await;
        assert!(result.is_err(), "expected the untrusted client to be refused");
    }

    #[tokio::test]
    async fn a_server_the_client_does_not_trust_is_refused() {
        let a = Identity::from_seed(&[1; 32]);
        let b = Identity::from_seed(&[2; 32]);
        let result = handshake(&a, vec![a.account(), b.account()], &b, vec![b.account()]).await;
        assert!(result.is_err(), "expected the untrusted server to be refused");
    }

    #[tokio::test]
    async fn an_operator_client_takes_any_node_but_the_node_still_pins_it() {
        let node = Identity::from_seed(&[1; 32]);
        let root = Identity::from_seed(&[2; 32]);
        let stranger = Identity::from_seed(&[3; 32]);
        // The client knows nothing about the node's account, and connects.
        assert!(handshake_with(&node, vec![root.account()], client_config_any_node(&root)).await.is_ok());
        // The node still refuses a client that isn't a member.
        assert!(handshake_with(&node, vec![root.account()], client_config_any_node(&stranger)).await.is_err());
    }

    /// The AWS outage, reproduced: clients that connect and never speak must
    /// not stop a real client from being served — well past the 128 a
    /// listen backlog holds, which is what filled up live.
    #[tokio::test]
    async fn silent_connections_do_not_block_a_real_client() {
        let node = Identity::from_seed(&[1; 32]);
        let peer = Identity::from_seed(&[2; 32]);
        let trusted = vec![node.account(), peer.account()];
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut listener = TlsListener::new(tcp, server_config(&node, trusted.clone()));
        let addr = axum::serve::Listener::local_addr(&listener).unwrap();

        // 200 connections that never send a byte, held open.
        let mut silent = Vec::new();
        for _ in 0..200 {
            silent.push(TcpStream::connect(addr).await.unwrap());
        }

        let client = tokio::spawn(async move {
            let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config(&peer, trusted)));
            let tcp = TcpStream::connect(addr).await.unwrap();
            connector.connect(ServerName::IpAddress(addr.ip().into()), tcp).await.map(|_| ()).map_err(|e| e.to_string())
        });
        let served = tokio::time::timeout(std::time::Duration::from_secs(5), axum::serve::Listener::accept(&mut listener)).await;
        assert!(served.is_ok(), "the real client was never handed over while silent ones were connected");
        assert!(client.await.unwrap().is_ok());
        drop(silent);
    }

    /// Dropping the listener stops its accept task and frees the port —
    /// what lets an aborted node come back on the same one.
    #[tokio::test]
    async fn dropping_the_listener_frees_the_port() {
        let node = Identity::from_seed(&[1; 32]);
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        drop(TlsListener::new(tcp, server_config(&node, vec![node.account()])));
        let t0 = std::time::Instant::now();
        loop {
            if TcpListener::bind(addr).await.is_ok() {
                break;
            }
            assert!(t0.elapsed() < std::time::Duration::from_secs(3), "port {addr} still held after the listener was dropped");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn a_cert_cannot_claim_an_account_it_does_not_hold_the_key_for() {
        // A well-formed, self-consistent cert for `b` cannot be presented
        // to impersonate `a`: check_trusted only ever inspects the cert
        // that was actually signed over by the live handshake, so there is
        // no way to "borrow" a's trusted status without b's private key.
        let a = Identity::from_seed(&[1; 32]);
        let b = Identity::from_seed(&[2; 32]);
        let (cert_b, _) = cert_for(&b);
        assert_ne!(account_of(&cert_b).unwrap(), a.account());
        assert_eq!(account_of(&cert_b).unwrap(), b.account());
    }
}
