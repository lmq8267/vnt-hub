use std::future::Future;
use std::io::Cursor;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::{Extension, Router};
use hyper::server::conn::Http;
use rustls_pemfile::{certs, pkcs8_private_keys, rsa_private_keys};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

pub type RawTcpHandler = Arc<
    dyn Fn(PrefixedStream, SocketAddr) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync,
>;

pub async fn serve_auto_tls(
    addr: SocketAddr,
    app: Router,
    cert_pem: Vec<u8>,
    key_pem: Vec<u8>,
) -> anyhow::Result<()> {
    serve_auto_tls_with_raw(addr, app, cert_pem, key_pem, None).await
}

pub async fn serve_auto_tls_with_raw(
    addr: SocketAddr,
    app: Router,
    cert_pem: Vec<u8>,
    key_pem: Vec<u8>,
    raw_tcp: Option<RawTcpHandler>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let tls = TlsAcceptor::from(Arc::new(tls_config(cert_pem, key_pem)?));
    loop {
        let (stream, peer) = listener.accept().await?;
        let app = app.clone();
        let tls = tls.clone();
        let raw_tcp = raw_tcp.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_one(peer, stream, app, tls, raw_tcp).await {
                log::warn!("connection {} failed {:?}", peer, e);
            }
        });
    }
}

async fn serve_one(
    peer: SocketAddr,
    stream: TcpStream,
    app: Router,
    tls: TlsAcceptor,
    raw_tcp: Option<RawTcpHandler>,
) -> anyhow::Result<()> {
    let app = app.layer(Extension(peer));
    let mut first = [0u8; 1];
    stream.readable().await?;
    let n = stream.try_read(&mut first)?;
    if n == 0 {
        return Ok(());
    }
    let prefixed = PrefixedStream::new(first[..n].to_vec(), stream);
    if first[0] == 0x16 {
        let tls_stream = tls.accept(prefixed).await?;
        Http::new()
            .serve_connection(tls_stream, app)
            .with_upgrades()
            .await?;
    } else if first[0] == b'{' {
        if let Some(handler) = raw_tcp {
            handler(prefixed, peer).await;
        } else {
            Http::new()
                .serve_connection(prefixed, app)
                .with_upgrades()
                .await?;
        }
    } else {
        Http::new()
            .serve_connection(prefixed, app)
            .with_upgrades()
            .await?;
    }
    Ok(())
}

fn tls_config(cert_pem: Vec<u8>, key_pem: Vec<u8>) -> anyhow::Result<ServerConfig> {
    let certs =
        certs(&mut Cursor::new(cert_pem)).collect::<Result<Vec<CertificateDer<'static>>, _>>()?;
    let key = load_private_key(key_pem)?;
    Ok(ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?)
}

fn load_private_key(key_pem: Vec<u8>) -> anyhow::Result<PrivateKeyDer<'static>> {
    if let Some(key) = pkcs8_private_keys(&mut Cursor::new(key_pem.clone())).next() {
        return Ok(key?.into());
    }
    if let Some(key) = rsa_private_keys(&mut Cursor::new(key_pem)).next() {
        return Ok(key?.into());
    }
    anyhow::bail!("tls private key missing")
}

pub struct PrefixedStream {
    prefix: Cursor<Vec<u8>>,
    inner: TcpStream,
}

impl PrefixedStream {
    fn new(prefix: Vec<u8>, inner: TcpStream) -> Self {
        Self {
            prefix: Cursor::new(prefix),
            inner,
        }
    }
}

impl AsyncRead for PrefixedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let pos = self.prefix.position() as usize;
        let prefix_len = self.prefix.get_ref().len();
        if pos < prefix_len {
            let src = &self.prefix.get_ref()[pos..];
            let len = src.len().min(buf.remaining());
            buf.put_slice(&src[..len]);
            self.prefix.set_position((pos + len) as u64);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for PrefixedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
