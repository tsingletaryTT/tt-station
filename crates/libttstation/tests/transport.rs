// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! HTTPS to a box with its certificate pinned (`libttstation::transport`),
//! against a throwaway in-process HTTPS server with a fresh self-signed
//! certificate -- the shape of a real box's TLS port, no agent needed.

use std::sync::Arc;

use libttstation::transport;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A tiny HTTPS "box": `/tls` reports `reported_sha` (its own fingerprint
/// unless a test makes it lie), `/status` answers idle. Returns its port and
/// the real fingerprint of the certificate it presents.
async fn https_box(lie_about_cert: bool) -> (u16, String) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_der = certified.cert.der().clone();
    let sha = transport::cert_sha256(cert_der.as_ref());
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into());
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert_der], key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let reported = if lie_about_cert {
        "00".repeat(32)
    } else {
        sha.clone()
    };
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            let reported = reported.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return; // the client refused the certificate
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let request = String::from_utf8_lossy(&buf);
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                let body = match path {
                    "/tls" => format!(r#"{{"tls":{{"port":{port},"sha256":"{reported}"}}}}"#),
                    _ => r#"{"status":"idle","device_mesh":null,"mac":null}"#.to_string(),
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = tls.write_all(response.as_bytes()).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    (port, sha)
}

#[tokio::test]
async fn probe_reports_the_certificate_the_box_presented() {
    let (port, sha) = https_box(false).await;
    let cert = transport::probe("127.0.0.1", port).await.unwrap();
    assert_eq!(cert.port, port);
    assert_eq!(cert.sha256, sha);
}

#[tokio::test]
async fn probe_refuses_a_box_whose_answer_disagrees_with_its_certificate() {
    let (port, _) = https_box(true).await;
    let err = transport::probe("127.0.0.1", port).await.unwrap_err();
    assert!(err.to_string().contains("answering for the box"), "{err:#}");
}

#[tokio::test]
async fn find_https_tries_the_next_port_up() {
    let (tls_port, sha) = https_box(false).await;
    // Plain HTTP on the port below, like the box's control port answering a
    // TLS hello it can't read.
    let Ok(plain) = tokio::net::TcpListener::bind(("127.0.0.1", tls_port - 1)).await else {
        return; // the port below is taken on this machine; nothing to show
    };
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = plain.accept().await {
            let _ = tcp.read(&mut [0u8; 512]).await;
            let _ = tcp
                .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n")
                .await;
        }
    });
    let cert = transport::find_https("127.0.0.1", tls_port - 1)
        .await
        .unwrap();
    assert_eq!(
        cert,
        transport::BoxCert {
            port: tls_port,
            sha256: sha
        }
    );
}

#[tokio::test]
async fn a_pinned_connection_works_with_the_right_certificate() {
    let (port, sha) = https_box(false).await;
    let base = format!("https://127.0.0.1:{port}");
    transport::pin(&format!("127.0.0.1:{port}"), &sha);
    let status = libttstation::agent_client::get_status(&base).await.unwrap();
    assert_eq!(status.status, libttstation::model::ServingStatus::Idle);
}

#[tokio::test]
async fn a_pinned_connection_refuses_a_changed_certificate_and_says_so() {
    let (port, sha) = https_box(false).await;
    let authority = format!("127.0.0.1:{port}");
    let pinned = "ab".repeat(32);
    transport::pin(&authority, &pinned);
    let err = libttstation::agent_client::get_status(&format!("https://{authority}"))
        .await
        .unwrap_err();
    let err = transport::explain(err);
    let changed = err
        .downcast_ref::<transport::CertificateChanged>()
        .expect("a certificate-changed error");
    assert_eq!(changed.authority, authority);
    assert_eq!(changed.expected, pinned);
    assert_eq!(changed.presented, sha);
    assert!(err.to_string().starts_with("certificate changed"), "{err}");
}

#[tokio::test]
async fn an_unpinned_https_box_is_not_trusted_by_default() {
    // No pin: the ordinary client checks the certificate against CAs, and a
    // box's self-signed certificate isn't trusted that way.
    let (port, _) = https_box(false).await;
    let base = format!("https://localhost:{port}");
    assert!(libttstation::agent_client::get_status(&base).await.is_err());
}
