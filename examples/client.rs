use std::sync::Arc;

use futures::future;
use h3_quinn::quinn;
use http_body::Body;
use tokio::io::AsyncWriteExt;
use tower_service::Service;

static ALPN: &[u8] = b"h3";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(std::io::stderr)
        .init();

    let dest = std::env::args().nth(1).expect("uri required via CLI").parse::<http::Uri>().unwrap();

    if dest.scheme() != Some(&http::uri::Scheme::HTTPS) {
        Err("destination scheme must be 'https'")?;
    }

    let auth = dest
        .authority()
        .ok_or("destination must have a host")?
        .clone();

    let port = auth.port_u16().unwrap_or(443);

    // dns me!
    let addr = tokio::net::lookup_host((auth.host(), port))
        .await?
        .next()
        .ok_or("dns found no addresses")?;

    eprintln!("DNS Lookup for {:?}: {:?}", dest, addr);

    // quinn setup
    let tls_config_builder = rustls::ClientConfig::builder()
        .with_safe_default_cipher_suites()
        .with_safe_default_kx_groups()
        .with_protocol_versions(&[&rustls::version::TLS13])?;
    let mut roots = rustls::RootCertStore::empty();
    match rustls_native_certs::load_native_certs() {
        Ok(certs) => {
            for cert in certs {
                if let Err(e) = roots.add(&rustls::Certificate(cert.0)) {
                    eprintln!("failed to parse trust anchor: {}", e);
                }
            }
        }
        Err(e) => {
            eprintln!("couldn't load any default trust roots: {}", e);
        }
    };
    let mut tls_config = tls_config_builder
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls_config.enable_early_data = true;
    tls_config.alpn_protocols = vec![ALPN.into()];
    let client_config = quinn::ClientConfig::new(Arc::new(tls_config));

    let mut client_endpoint = h3_quinn::quinn::Endpoint::client("[::]:0".parse().unwrap())?;
    client_endpoint.set_default_client_config(client_config);
    let quinn_conn = h3_quinn::Connection::new(client_endpoint.connect(addr, auth.host())?.await?);

    eprintln!("QUIC connected ...");

    // generic h3
    let (mut driver, mut send_request) = h3::client::new(quinn_conn).await?;

    tokio::spawn(async move {
        if let Err(e) = future::poll_fn(|cx| driver.poll_close(cx)).await {
            eprintln!("h3 connection driver error: {}", e);
        }
    });

    let mut svc = tower_h3::client::Connection::new(send_request);

    eprintln!("Sending request ...");
    let req = http::Request::builder()
        .uri(dest)
        .body(http_body::Empty::new())?;

    let mut resp = svc.call(req).await?;

    eprintln!("Received response ...");

    eprintln!("Response: {:?} {}", resp.version(), resp.status());
    eprintln!("Headers: {:#?}", resp.headers());

    while let Some(next) = resp.body_mut().data().await {
        let mut chunk = next?;
        let mut out = tokio::io::stdout();
        out.write_all_buf(&mut chunk).await.expect("write_all");
        out.flush().await.expect("flush");
    }

    drop(svc);
    client_endpoint.wait_idle().await;

    Ok(())
}
