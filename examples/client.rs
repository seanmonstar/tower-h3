use std::error::Error;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::future::{self, Future};
use http_body::Body;
use tokio::io::AsyncWriteExt;
use tower_h3::client::Endpoint;
use tower_service::Service;

static ALPN: &[u8] = b"h3";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(std::io::stderr)
        .init();

    let dest = std::env::args()
        .nth(1)
        .expect("uri required via CLI")
        .parse::<http::Uri>()
        .unwrap();

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
    let mut quinn_endpoint = quinn::Endpoint::client("[::]:0".parse().unwrap())?;
    quinn_endpoint.set_default_client_config(client_config);

    let conn = Connect::new(quinn_endpoint.clone());

    // TODO make type inference work and remove this
    let mut endpoint: Endpoint<
        Connect,
        http_body::Empty<bytes::Bytes>,
    > = Endpoint::new(conn);
    let (mut driver, mut service) = endpoint.call((addr, auth.host())).await?;

    eprintln!("QUIC connected ...");

    tokio::spawn(async move {
        if let Err(e) = future::poll_fn(|cx| driver.poll_close(cx)).await {
            eprintln!("h3 connection driver error: {}", e);
        }
    });

    eprintln!("Sending request ...");
    let req = http::Request::builder()
        .uri(dest)
        .body(http_body::Empty::new())?;

    let mut resp = service.call(req).await?;

    eprintln!("Received response ...");

    eprintln!("Response: {:?} {}", resp.version(), resp.status());
    eprintln!("Headers: {:#?}", resp.headers());

    while let Some(next) = resp.body_mut().data().await {
        let mut chunk = next?;
        let mut out = tokio::io::stdout();
        out.write_all_buf(&mut chunk).await.expect("write_all");
        out.flush().await.expect("flush");
    }

    drop(service);
    quinn_endpoint.wait_idle().await;

    Ok(())
}

pub struct Connect {
    inner: h3_quinn::quinn::Endpoint,
 }

 impl Connect {
     pub fn new(inner: h3_quinn::quinn::Endpoint) -> Self {
         Self {
             inner,
         }
     }
 }

 impl Service<(SocketAddr, &str)> for Connect {
     type Response = h3_quinn::Connection;
     type Error = Box<dyn Error>;
     type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

     fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
         Poll::Ready(Ok(()))
     }

     fn call(&mut self, dst: (SocketAddr, &str)) -> Self::Future {
         let (addr, server_name) = dst;
         let connecting = self.inner.connect(addr, server_name);

         Box::pin(async move {
             let conn = connecting?.await?;
             Ok(h3_quinn::Connection::new(conn))
         })
     }
 }
