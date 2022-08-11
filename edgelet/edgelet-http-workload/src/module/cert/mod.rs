// Copyright (c) Microsoft. All rights reserved.

mod cert_key;
use cert_key::CertKey;

mod dps_policy;

pub(crate) mod identity;
pub(crate) mod server;

#[cfg(not(test))]
use aziot_cert_client_async::Client as CertClient;
#[cfg(not(test))]
use aziot_key_client_async::Client as KeyClient;

#[cfg(test)]
use test_common::client::CertClient;
#[cfg(test)]
use test_common::client::KeyClient;

use aziot_identity_common_http::get_provisioning_info::Response as ProvisioningInfo;

#[derive(Debug, serde::Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(tag = "type")]
pub(crate) enum PrivateKey {
    #[serde(rename = "key")]
    Key { bytes: String },
}

#[derive(Debug, serde::Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
pub(crate) struct CertificateResponse {
    #[serde(rename = "privateKey")]
    private_key: PrivateKey,

    certificate: String,
    expiration: String,
}

pub(crate) enum SubjectAltName {
    Dns(String),
    Ip(String),
}

struct CertApi {
    key_client: std::sync::Arc<futures_util::lock::Mutex<KeyClient>>,
    cert_client: std::sync::Arc<futures_util::lock::Mutex<CertClient>>,
    key_connector: http_common::Connector,

    edge_ca_cert: String,
    edge_ca_key: String,
}

impl CertApi {
    pub fn new(
        key_client: std::sync::Arc<futures_util::lock::Mutex<KeyClient>>,
        cert_client: std::sync::Arc<futures_util::lock::Mutex<CertClient>>,
        key_connector: http_common::Connector,
        config: &crate::WorkloadConfig,
    ) -> Self {
        CertApi {
            key_client,
            cert_client,
            key_connector,
            edge_ca_cert: config.edge_ca_cert.clone(),
            edge_ca_key: config.edge_ca_key.clone(),
        }
    }

    pub async fn cert_from_edge_ca(
        self,
        cert_id: String,
        common_name: String,
        subject_alt_names: Vec<SubjectAltName>,
        extensions: openssl::stack::Stack<openssl::x509::X509Extension>,
    ) -> Result<hyper::Response<hyper::Body>, http_common::server::Error> {
        // In the future, we may need to support configurable key type. For now, default to
        // RSA 2048 to maintain consistent behavior with prior versions of Edge.
        let keys = CertKey::Rsa(2048)
            .generate()
            .map_err(|_| edgelet_http::error::server_error("failed to generate csr keys"))?;
        let private_key = key_to_pem(&keys.0);

        let mut subject = openssl::x509::X509Name::builder()
            .map_err(|_| edgelet_http::error::server_error("failed to generate csr subject"))?;
        subject
            .append_entry_by_nid(openssl::nid::Nid::COMMONNAME, &common_name)
            .map_err(|_| edgelet_http::error::server_error("failed to generate csr subject"))?;
        let subject = subject.build();

        let csr = new_csr(&subject, keys, subject_alt_names, extensions)
            .map_err(|_| edgelet_http::error::server_error("failed to generate csr"))?;
        let csr = csr
            .to_pem()
            .map_err(|_| edgelet_http::error::server_error("failed to serialize csr"))?;

        let edge_ca_key_handle = {
            let key_client = self.key_client.lock().await;

            key_client
                .load_key_pair(&self.edge_ca_key)
                .await
                .map_err(|_| edgelet_http::error::server_error("failed to get edge CA key"))?
        };

        let cert = self
            .create_cert(&cert_id, &csr, &edge_ca_key_handle)
            .await?;

        let expiration = get_expiration(&cert)?;

        let response = CertificateResponse {
            private_key: PrivateKey::Key { bytes: private_key },
            certificate: cert,
            expiration,
        };
        let response = http_common::server::response::json(hyper::StatusCode::CREATED, &response);

        Ok(response)
    }

    pub async fn cert_from_dps(
        self,
        policy: aziot_identity_common_http::get_provisioning_info::Response,
        subject_alt_names: Vec<SubjectAltName>,
        extensions: openssl::stack::Stack<openssl::x509::X509Extension>,
    ) -> Result<hyper::Response<hyper::Body>, http_common::server::Error> {
        let (identity_cert, identity_private_key) = self.get_dps_credentials().await?;

        let subject = {
            if let ProvisioningInfo::Dps {
                registration_id, ..
            } = &policy
            {
                let mut subject = openssl::x509::X509Name::builder().map_err(|_| {
                    edgelet_http::error::server_error("failed to generate csr subject")
                })?;
                subject
                    .append_entry_by_nid(openssl::nid::Nid::COMMONNAME, registration_id)
                    .map_err(|_| {
                        edgelet_http::error::server_error("failed to generate csr subject")
                    })?;

                subject.build()
            } else {
                // This function is only called after DPS policy is checked.
                unreachable!()
            }
        };

        // In the future, we may need to support configurable key type. For now, default to
        // EC P-256, which should be supported by all DPS-connected CAs.
        let keys = CertKey::Ec(openssl::nid::Nid::X9_62_PRIME256V1)
            .generate()
            .map_err(|_| edgelet_http::error::server_error("failed to generate csr keys"))?;
        let server_cert_private_key = key_to_pem(&keys.0);

        // DPS sets the certificate extensions and SANs. Therefore, these fields do not need
        // to be in the CSR.
        let csr = new_csr(&subject, keys, subject_alt_names, extensions)
            .map_err(|_| edgelet_http::error::server_error("failed to generate csr"))?;

        let dps_request = aziot_cloud_client_async::dps::IssueCert::new(
            policy,
            identity_cert,
            identity_private_key,
        );

        let cert = dps_request.issue_server_cert(csr).await.map_err(|err| {
            edgelet_http::error::server_error(format!(
                "failed to issue server certificate from DPS: {}",
                err
            ))
        })?;

        let cert = String::from_utf8(cert).map_err(|_| {
            edgelet_http::error::server_error("failed to parse server certificate as utf-8")
        })?;
        let expiration = get_expiration(&cert)?;

        let response = CertificateResponse {
            private_key: PrivateKey::Key {
                bytes: server_cert_private_key,
            },
            certificate: cert,
            expiration,
        };
        let response = http_common::server::response::json(hyper::StatusCode::CREATED, &response);

        Ok(response)
    }

    async fn create_cert(
        &self,
        cert_id: &str,
        csr: &[u8],
        edge_ca_key_handle: &aziot_key_common::KeyHandle,
    ) -> Result<String, http_common::server::Error> {
        let cert = {
            let cert_client = self.cert_client.lock().await;

            cert_client
                .create_cert(cert_id, csr, Some((&self.edge_ca_cert, edge_ca_key_handle)))
                .await
                .map_err(|_| {
                    edgelet_http::error::server_error(format!("failed to create cert {}", cert_id))
                })
        }?;

        let cert = std::str::from_utf8(&cert)
            .map_err(|_| edgelet_http::error::server_error("invalid cert created"))?;

        Ok(cert.to_string())
    }

    async fn get_dps_credentials(
        &self,
    ) -> Result<(Vec<u8>, openssl::pkey::PKey<openssl::pkey::Private>), http_common::server::Error>
    {
        let private_key = {
            let key_client = self.key_client.lock().await;

            let key_handle = key_client
                .load_key_pair(aziot_identity_common::DPS_IDENTITY_CERT_KEY)
                .await
                .map_err(|err| {
                    edgelet_http::error::server_error(format!(
                        "failed to get DPS identity cert key: {}",
                        err
                    ))
                })?;

            let (private_key, _) = crate::edge_ca::keys(self.key_connector.clone(), &key_handle)
                .map_err(edgelet_http::error::server_error)?;

            private_key
        };

        let cert = {
            let cert_client = self.cert_client.lock().await;

            cert_client
                .get_cert(aziot_identity_common::DPS_IDENTITY_CERT)
                .await
                .map_err(|err| {
                    edgelet_http::error::server_error(format!(
                        "failed to get DPS identity cert: {}",
                        err
                    ))
                })?
        };

        // TODO: need to renew DPS identity cert if expired.

        Ok((cert, private_key))
    }
}

pub(crate) fn new_csr(
    subject: &openssl::x509::X509NameRef,
    keys: (
        openssl::pkey::PKey<openssl::pkey::Private>,
        openssl::pkey::PKey<openssl::pkey::Public>,
    ),
    subject_alt_names: Vec<SubjectAltName>,
    mut extensions: openssl::stack::Stack<openssl::x509::X509Extension>,
) -> Result<openssl::x509::X509Req, openssl::error::ErrorStack> {
    let private_key = keys.0;
    let public_key = keys.1;

    let mut csr = openssl::x509::X509Req::builder()?;
    csr.set_version(0)?;
    csr.set_subject_name(subject)?;

    csr.set_pubkey(&public_key)?;

    if !subject_alt_names.is_empty() {
        let mut names = openssl::x509::extension::SubjectAlternativeName::new();

        for name in subject_alt_names {
            match name {
                SubjectAltName::Dns(name) => names.dns(&name),
                SubjectAltName::Ip(name) => names.ip(&name),
            };
        }

        let names = names.build(&csr.x509v3_context(None))?;
        extensions.push(names)?;
    }

    csr.add_extensions(&extensions)?;

    csr.sign(&private_key, openssl::hash::MessageDigest::sha256())?;

    let csr = csr.build();

    Ok(csr)
}

fn get_expiration(cert: &str) -> Result<String, http_common::server::Error> {
    let cert = openssl::x509::X509::from_pem(cert.as_bytes())
        .map_err(|_| edgelet_http::error::server_error("failed to parse cert"))?;

    // openssl::asn1::Asn1TimeRef does not expose any way to convert the ASN1_TIME to a Rust-friendly type
    //
    // Its Display impl uses ASN1_TIME_print, so we convert it into a String and parse it back
    // into a chrono::DateTime<chrono::Utc>
    let expiration = cert.not_after().to_string();
    let expiration = chrono::NaiveDateTime::parse_from_str(&expiration, "%b %e %H:%M:%S %Y GMT")
        .expect("cert not_after should parse");
    let expiration = chrono::DateTime::<chrono::Utc>::from_utc(expiration, chrono::Utc);

    Ok(expiration.to_rfc3339())
}

fn key_to_pem(key: &openssl::pkey::PKey<openssl::pkey::Private>) -> String {
    // The key parameter is always generated by this library. It should be valid.
    let key_pem = key.private_key_to_pem_pkcs8().expect("key is invalid");

    let key_pem = std::str::from_utf8(&key_pem)
        .expect("key is invalid")
        .to_string();

    key_pem
}

#[cfg(test)]
mod tests {
    fn test_api() -> super::CertApi {
        // Tests won't actually connect to keyd, so just put any URL in the key connector.
        let key_connector = url::Url::parse("unix:///tmp/test.sock").unwrap();
        let key_connector = http_common::Connector::new(&key_connector).unwrap();

        let key_client = super::KeyClient::default();
        let key_client = std::sync::Arc::new(futures_util::lock::Mutex::new(key_client));

        let cert_client = super::CertClient::default();
        let cert_client = std::sync::Arc::new(futures_util::lock::Mutex::new(cert_client));

        super::CertApi {
            key_client,
            cert_client,
            key_connector,

            edge_ca_cert: "test-device-cert".to_string(),
            edge_ca_key: "test-device-key".to_string(),
        }
    }

    #[tokio::test]
    async fn issue_cert() {
        let api = test_api();

        let (_, issuer_key) = {
            let cert_client = api.cert_client.lock().await;
            (cert_client.issuer.clone(), cert_client.issuer_key.clone())
        };

        // It doesn't matter what extensions we use for this test, so just use an empty stack.
        let extensions = openssl::stack::Stack::new().unwrap();

        let response = api
            .cert_from_edge_ca(
                "testCertificate".to_string(),
                "testCertificate".to_string(),
                // This test won't check these fields, so it doesn't matter what's passed here.
                vec![],
                extensions,
            )
            .await
            .unwrap();

        // Parse response
        let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
        let response: super::CertificateResponse = serde_json::from_slice(&body).unwrap();

        let cert = openssl::x509::X509::from_pem(response.certificate.as_bytes()).unwrap();
        let private_key = match response.private_key {
            super::PrivateKey::Key { bytes } => {
                openssl::pkey::PKey::private_key_from_pem(bytes.as_bytes()).unwrap()
            }
        };
        let expiration = chrono::DateTime::parse_from_rfc3339(&response.expiration).unwrap();

        // Check that expiration is in the future.
        assert!(expiration > chrono::offset::Utc::now());

        // Check that private key in response matches certificate.
        let public_key = private_key.public_key_to_pem().unwrap();
        let cert_public_key = cert.public_key().unwrap().public_key_to_pem().unwrap();
        assert_eq!(cert_public_key, public_key);

        // Check certificate is signed by issuer key.
        assert!(cert.verify(&issuer_key).unwrap());
    }
}
