//! TLS of a data source (the SSL part of its SSH/SSL tab) for PostgreSQL and MySQL/MariaDB: how
//! the connection is encrypted and checked, the CA to trust and the client's own certificate.
//!
//! The files are read on every connect, so a renewed certificate needs no edit. The client key
//! may be PKCS#8, PKCS#1 or SEC1 PEM; an encrypted PKCS#8 key takes its passphrase, which is kept
//! in the keychain.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio_rustls::rustls::client::WebPkiServerVerifier;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::{
    CryptoProvider, verify_tls12_signature, verify_tls13_signature,
};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime,
};
use tokio_rustls::rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};

use super::Driver;
use crate::i18n::{tr, trf};

/// libpq's `sslmode`, as DataGrip offers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum TlsMode {
    /// Encrypted when the server offers it, plain otherwise; the certificate isn't checked.
    #[default]
    Prefer,
    Disable,
    /// Encrypted, certificate not checked.
    Require,
    /// Encrypted, certificate signed by a trusted CA, for any host name.
    VerifyCa,
    /// Encrypted, certificate signed by a trusted CA and issued for the host.
    VerifyFull,
}

impl TlsMode {
    pub const ALL: [TlsMode; 5] = [
        TlsMode::Prefer,
        TlsMode::Disable,
        TlsMode::Require,
        TlsMode::VerifyCa,
        TlsMode::VerifyFull,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TlsMode::Prefer => tr("Prefer"),
            TlsMode::Disable => tr("Disable"),
            TlsMode::Require => tr("Require"),
            TlsMode::VerifyCa => tr("Verify CA"),
            TlsMode::VerifyFull => tr("Verify full"),
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            TlsMode::Prefer => {
                tr("Encrypted if the server offers it; the certificate isn't checked")
            }
            TlsMode::Disable => tr("Never encrypted"),
            TlsMode::Require => tr("Always encrypted; the certificate isn't checked"),
            TlsMode::VerifyCa => tr("Always encrypted, with a certificate signed by a trusted CA"),
            TlsMode::VerifyFull => tr(
                "Always encrypted, with a certificate signed by a trusted CA and issued for the host",
            ),
        }
    }
}

/// The SSL part of a data source's SSH/SSL tab.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TlsSettings {
    pub mode: TlsMode,
    /// The CA certificates to trust (PEM). Empty: the system's.
    pub ca_file: String,
    /// The client's certificate and key, when the server signs users in by certificate.
    pub cert_file: String,
    pub key_file: String,
    /// In the keychain.
    #[serde(skip)]
    pub key_passphrase: String,
}

/// Engines whose TLS the SSL settings configure.
pub fn supported(driver: Driver) -> bool {
    matches!(driver, Driver::Postgres | Driver::MySql | Driver::MariaDb)
}

/// `~/…` is the home folder.
pub fn path(file: &str) -> PathBuf {
    let file = file.trim();
    match (
        file.strip_prefix("~/").or_else(|| file.strip_prefix("~\\")),
        dirs::home_dir(),
    ) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(file),
    }
}

/// The certificates of a PEM file.
pub fn certificates(file: &str) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let p = path(file);
    let bytes = std::fs::read(&p).map_err(|e| {
        anyhow!(trf(
            "Could not read {file}: {error}",
            &[("file", &p.display()), ("error", &e)]
        ))
    })?;
    let certs: Vec<_> = CertificateDer::pem_slice_iter(&bytes)
        .collect::<Result<_, _>>()
        .map_err(|e| {
            anyhow!(trf(
                "{file} is not a PEM certificate file: {error}",
                &[("file", &p.display()), ("error", &e)]
            ))
        })?;
    if certs.is_empty() {
        bail!(trf("{file} has no certificate", &[("file", &p.display())]));
    }
    Ok(certs)
}

/// The client key: PEM (PKCS#8, PKCS#1 or SEC1), or encrypted PKCS#8 with its passphrase.
pub fn private_key(file: &str, passphrase: &str) -> anyhow::Result<PrivateKeyDer<'static>> {
    let p = path(file);
    let bytes = std::fs::read(&p).map_err(|e| {
        anyhow!(trf(
            "Could not read {file}: {error}",
            &[("file", &p.display()), ("error", &e)]
        ))
    })?;
    let text = String::from_utf8_lossy(&bytes);
    if text.contains("Proc-Type: 4,ENCRYPTED") {
        bail!(tr(
            "Legacy encrypted PEM keys are not supported; convert it with: openssl pkcs8 -topk8 -v2 aes256 -in key.pem -out key.p8"
        ));
    }
    if text.contains("-----BEGIN ENCRYPTED PRIVATE KEY-----") {
        if passphrase.is_empty() {
            bail!(tr("The client key is protected by a passphrase: enter it"));
        }
        let body: String = text
            .lines()
            .skip_while(|l| {
                !l.trim()
                    .starts_with("-----BEGIN ENCRYPTED PRIVATE KEY-----")
            })
            .skip(1)
            .take_while(|l| !l.trim().starts_with("-----END "))
            .flat_map(|l| l.chars().filter(|c| !c.is_whitespace()))
            .collect();
        let der = base64::engine::general_purpose::STANDARD
            .decode(body)
            .map_err(|e| anyhow!(trf("Invalid client key: {error}", &[("error", &e)])))?;
        let info = pkcs8::EncryptedPrivateKeyInfo::try_from(der.as_slice())
            .map_err(|e| anyhow!(trf("Invalid client key: {error}", &[("error", &e)])))?;
        let plain = info
            .decrypt(passphrase)
            .map_err(|_| anyhow!(tr("Could not decrypt the client key: wrong passphrase?")))?;
        return Ok(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            plain.as_bytes().to_vec(),
        )));
    }
    PrivateKeyDer::from_pem_slice(&bytes).map_err(|e| {
        anyhow!(trf(
            "{file} is not a PEM private key: {error}",
            &[("file", &p.display()), ("error", &e)]
        ))
    })
}

/// The client key as PEM for drivers that read it themselves (mysql_async): the file as it is,
/// or decrypted to PKCS#8 when it is protected by a passphrase.
pub fn private_key_pem(file: &str, passphrase: &str) -> anyhow::Result<Vec<u8>> {
    let key = private_key(file, passphrase)?;
    match key {
        PrivateKeyDer::Pkcs8(der) if passphrase_protected(file) => {
            let body = base64::engine::general_purpose::STANDARD.encode(der.secret_pkcs8_der());
            let lines: Vec<&str> = body
                .as_bytes()
                .chunks(64)
                .map(|c| std::str::from_utf8(c).unwrap_or_default())
                .collect();
            Ok(format!(
                "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
                lines.join("\n")
            )
            .into_bytes())
        }
        _ => Ok(std::fs::read(path(file))?),
    }
}

fn passphrase_protected(file: &str) -> bool {
    std::fs::read_to_string(path(file))
        .is_ok_and(|t| t.contains("-----BEGIN ENCRYPTED PRIVATE KEY-----"))
}

/// The rustls client for these settings (never called for `Disable`).
pub fn client_config(s: &TlsSettings) -> anyhow::Result<ClientConfig> {
    let builder = ClientConfig::builder();
    let provider = builder.crypto_provider().clone();
    let verifier: Arc<dyn ServerCertVerifier> = match s.mode {
        TlsMode::Prefer | TlsMode::Disable | TlsMode::Require => {
            Arc::new(AcceptAnyCertificate(provider))
        }
        TlsMode::VerifyCa | TlsMode::VerifyFull => {
            let trusted: Arc<dyn ServerCertVerifier> = if s.ca_file.trim().is_empty() {
                Arc::new(rustls_platform_verifier::Verifier::new(provider)?)
            } else {
                let mut roots = RootCertStore::empty();
                for cert in certificates(&s.ca_file)? {
                    roots.add(cert).map_err(|e| {
                        anyhow!(trf("Invalid CA certificate: {error}", &[("error", &e)]))
                    })?;
                }
                WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider).build()?
            };
            if s.mode == TlsMode::VerifyCa {
                Arc::new(AnyHostName(trusted))
            } else {
                trusted
            }
        }
    };
    let builder = builder
        .dangerous()
        .with_custom_certificate_verifier(verifier);
    let has_cert = !s.cert_file.trim().is_empty();
    let has_key = !s.key_file.trim().is_empty();
    Ok(match (has_cert, has_key) {
        (true, true) => builder
            .with_client_auth_cert(
                certificates(&s.cert_file)?,
                private_key(&s.key_file, &s.key_passphrase)?,
            )
            .map_err(|e| {
                anyhow!(trf(
                    "The client certificate and key don't match: {error}",
                    &[("error", &e)]
                ))
            })?,
        (false, false) => builder.with_no_client_auth(),
        (true, false) => bail!(tr("Choose the client key of the client certificate")),
        (false, true) => bail!(tr("Choose the client certificate of the client key")),
    })
}

/// A readable reason when `error` (or what caused it) is a refused server certificate: which CA
/// didn't sign it, or which host it isn't for.
pub fn explain(
    error: &(dyn std::error::Error + 'static),
    s: &TlsSettings,
    host: &str,
) -> Option<String> {
    let mut current = Some(error);
    while let Some(e) = current {
        let rustls = e.downcast_ref::<tokio_rustls::rustls::Error>().or_else(|| {
            e.downcast_ref::<std::io::Error>()
                .and_then(|io| io.get_ref())
                .and_then(|inner| inner.downcast_ref::<tokio_rustls::rustls::Error>())
        });
        if let Some(tokio_rustls::rustls::Error::InvalidCertificate(cert)) = rustls {
            let ca = if s.ca_file.trim().is_empty() {
                tr("a certificate authority this computer trusts").to_string()
            } else {
                s.ca_file.trim().to_string()
            };
            return Some(match cert {
                // BadSignature: an issuer of the same name, but another key.
                CertificateError::UnknownIssuer | CertificateError::BadSignature => trf(
                    "The server's certificate isn't signed by {ca}",
                    &[("ca", &ca)],
                ),
                CertificateError::NotValidForName
                | CertificateError::NotValidForNameContext { .. } => trf(
                    "The server's certificate isn't issued for {host}: use Verify CA, or the name it was issued for",
                    &[("host", &host)],
                ),
                CertificateError::Expired | CertificateError::ExpiredContext { .. } => {
                    tr("The server's certificate has expired").to_string()
                }
                other => trf(
                    "The server's certificate was refused: {error}",
                    &[("error", &format!("{other:?}"))],
                ),
            });
        }
        current = e.source();
    }
    None
}

/// No certificate check (Prefer, Require): any certificate is accepted, but the handshake
/// signatures are still verified.
#[derive(Debug)]
pub struct AcceptAnyCertificate(pub Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, tokio_rustls::rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Verify CA: the chain must lead to a trusted CA, whatever host name the certificate names.
#[derive(Debug)]
struct AnyHostName(Arc<dyn ServerCertVerifier>);

impl ServerCertVerifier for AnyHostName {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, tokio_rustls::rustls::Error> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
            Err(tokio_rustls::rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// A CA, a certificate for `localhost` and a client certificate, made for the tests, and TLS
/// servers on 127.0.0.1 inside the test process.
#[cfg(test)]
pub mod test_pki {
    use super::*;
    use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, DnType, IsCa, KeyPair};
    use tokio::io::{AsyncRead, AsyncWrite};
    use tokio::net::TcpListener;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::server::WebPkiClientVerifier;

    pub struct Pki {
        pub dir: PathBuf,
        pub ca_file: String,
        pub client_cert: String,
        pub client_key: String,
        /// The client key encrypted with the passphrase "pass phrase".
        pub client_key_encrypted: String,
        server: (CertificateDer<'static>, Vec<u8>),
        ca: CertificateDer<'static>,
    }

    impl Drop for Pki {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    pub fn pki() -> Pki {
        let dir = std::env::temp_dir().join(format!("datacraft-tls-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "DataCraft Test CA");
        let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate().unwrap()).unwrap();
        let server_key = KeyPair::generate().unwrap();
        let server = CertificateParams::new(vec!["localhost".to_string()])
            .unwrap()
            .signed_by(&server_key, &ca)
            .unwrap();
        let client_key = KeyPair::generate().unwrap();
        let mut client_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        client_params
            .distinguished_name
            .push(DnType::CommonName, "deploy");
        let client = client_params.signed_by(&client_key, &ca).unwrap();

        let write = |name: &str, text: &str| {
            let p = dir.join(name);
            std::fs::write(&p, text).unwrap();
            p.to_string_lossy().into_owned()
        };
        let salt = [7u8; 16];
        let iv = [9u8; 16];
        let params =
            pkcs8::pkcs5::pbes2::Parameters::pbkdf2_sha256_aes256cbc(2048, &salt, &iv).unwrap();
        let encrypted = pkcs8::PrivateKeyInfo::try_from(client_key.serialize_der().as_slice())
            .unwrap()
            .encrypt_with_params(params, "pass phrase")
            .unwrap();
        let body = base64::engine::general_purpose::STANDARD.encode(encrypted.as_bytes());
        let lines: Vec<&str> = body
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        Pki {
            ca_file: write("ca.pem", &ca.pem()),
            client_cert: write("client.pem", &client.pem()),
            client_key: write("client.key", &client_key.serialize_pem()),
            client_key_encrypted: write(
                "client-encrypted.key",
                &format!(
                    "-----BEGIN ENCRYPTED PRIVATE KEY-----\n{}\n-----END ENCRYPTED PRIVATE KEY-----\n",
                    lines.join("\n")
                ),
            ),
            server: (server.der().clone(), server_key.serialize_der()),
            ca: ca.der().clone(),
            dir,
        }
    }

    impl Pki {
        /// The server side: TLS with the `localhost` certificate, asking for a client certificate
        /// signed by the CA when `client_auth`.
        pub fn acceptor(&self, client_auth: bool) -> tokio_rustls::TlsAcceptor {
            let builder = ServerConfig::builder();
            let builder = if client_auth {
                let mut roots = RootCertStore::empty();
                roots.add(self.ca.clone()).unwrap();
                builder.with_client_cert_verifier(
                    WebPkiClientVerifier::builder(Arc::new(roots))
                        .build()
                        .unwrap(),
                )
            } else {
                builder.with_no_client_auth()
            };
            let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.server.1.clone()));
            tokio_rustls::TlsAcceptor::from(Arc::new(
                builder
                    .with_single_cert(vec![self.server.0.clone()], key)
                    .unwrap(),
            ))
        }

        /// A TLS server that greets every client with "ok"; its port.
        pub async fn serve(&self, client_auth: bool) -> u16 {
            let acceptor = self.acceptor(client_auth);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move {
                while let Ok((tcp, _)) = listener.accept().await {
                    let acceptor = acceptor.clone();
                    tokio::spawn(async move {
                        if let Ok(mut tls) = acceptor.accept(tcp).await {
                            let _ = tokio::io::AsyncWriteExt::write_all(&mut tls, b"ok").await;
                            let _ = tokio::io::AsyncWriteExt::shutdown(&mut tls).await;
                        }
                    });
                }
            });
            port
        }
    }

    /// Connects with the settings' client, as `name`, and reads the greeting.
    pub async fn handshake(s: &TlsSettings, name: &str, port: u16) -> anyhow::Result<()> {
        let config = client_config(s)?;
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let result: std::io::Result<()> = async {
            let mut tls = connector
                .connect(ServerName::try_from(name.to_string()).unwrap(), tcp)
                .await?;
            read_ok(&mut tls).await
        }
        .await;
        result.map_err(|e| match explain(&e, s, name) {
            Some(reason) => anyhow!(reason),
            None => anyhow!(e),
        })
    }

    async fn read_ok<S: AsyncRead + AsyncWrite + Unpin>(tls: &mut S) -> std::io::Result<()> {
        let mut buf = [0; 2];
        tokio::io::AsyncReadExt::read_exact(tls, &mut buf).await?;
        if &buf != b"ok" {
            return Err(std::io::Error::other("unexpected greeting"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::test_pki::*;
    use super::*;

    fn settings(mode: TlsMode, ca_file: &str) -> TlsSettings {
        TlsSettings {
            mode,
            ca_file: ca_file.into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn modes_check_what_they_promise() {
        let pki = pki();
        let port = pki.serve(false).await;
        handshake(
            &settings(TlsMode::VerifyFull, &pki.ca_file),
            "localhost",
            port,
        )
        .await
        .unwrap();
        // Another name: Verify full refuses it and says for which host; Verify CA accepts it.
        let err = handshake(
            &settings(TlsMode::VerifyFull, &pki.ca_file),
            "db.example",
            port,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("isn't issued for db.example"), "{err}");
        handshake(
            &settings(TlsMode::VerifyCa, &pki.ca_file),
            "db.example",
            port,
        )
        .await
        .unwrap();
        // A CA this computer doesn't trust, nor the file names.
        let err = handshake(&settings(TlsMode::VerifyFull, ""), "localhost", port)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(tr("a certificate authority this computer trusts")),
            "{err}"
        );
        let other = test_pki::pki();
        let err = handshake(
            &settings(TlsMode::VerifyCa, &other.ca_file),
            "localhost",
            port,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains(&other.ca_file), "{err}");
        // Require and Prefer don't check.
        handshake(&settings(TlsMode::Require, ""), "db.example", port)
            .await
            .unwrap();
        handshake(&settings(TlsMode::Prefer, ""), "db.example", port)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn client_certificates() {
        let pki = pki();
        let port = pki.serve(true).await;
        let mut s = settings(TlsMode::VerifyFull, &pki.ca_file);
        assert!(
            handshake(&s, "localhost", port).await.is_err(),
            "the server asks for a certificate"
        );
        s.cert_file = pki.client_cert.clone();
        s.key_file = pki.client_key.clone();
        handshake(&s, "localhost", port).await.unwrap();

        // An encrypted PKCS#8 key: its passphrase is needed, and must be right.
        s.key_file = pki.client_key_encrypted.clone();
        let err = handshake(&s, "localhost", port)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            tr("The client key is protected by a passphrase: enter it")
        );
        s.key_passphrase = "wrong".into();
        assert_eq!(
            handshake(&s, "localhost", port)
                .await
                .unwrap_err()
                .to_string(),
            tr("Could not decrypt the client key: wrong passphrase?")
        );
        s.key_passphrase = "pass phrase".into();
        handshake(&s, "localhost", port).await.unwrap();
        // As PEM for mysql_async: decrypted.
        let pem =
            String::from_utf8(private_key_pem(&s.key_file, &s.key_passphrase).unwrap()).unwrap();
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----"), "{pem}");
        assert!(PrivateKeyDer::from_pem_slice(pem.as_bytes()).is_ok());

        // Half a client identity, and missing files.
        s.key_file.clear();
        assert_eq!(
            client_config(&s).unwrap_err().to_string(),
            tr("Choose the client key of the client certificate")
        );
        s.cert_file = pki.dir.join("missing.pem").to_string_lossy().into_owned();
        s.key_file = pki.client_key.clone();
        assert!(
            client_config(&s)
                .unwrap_err()
                .to_string()
                .contains("missing.pem")
        );
        let ca_missing = settings(
            TlsMode::VerifyCa,
            &pki.dir.join("nope.pem").to_string_lossy(),
        );
        assert!(
            client_config(&ca_missing)
                .unwrap_err()
                .to_string()
                .contains("nope.pem")
        );
    }

    #[test]
    fn engines_and_paths() {
        assert!(
            supported(Driver::Postgres) && supported(Driver::MySql) && supported(Driver::MariaDb)
        );
        assert!(!supported(Driver::Sqlite) && !supported(Driver::MsSql));
        if let Some(home) = dirs::home_dir() {
            assert_eq!(path("~/certs/ca.pem"), home.join("certs").join("ca.pem"));
        }
    }
}
