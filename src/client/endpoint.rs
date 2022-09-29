use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::future::Future;
use h3::quic;
use http_body::Body;
use tower_service::Service;

pub struct Endpoint<C, B> {
    inner: C,
    _body: PhantomData<fn() -> fn(B)>,
}

impl<C, B> Endpoint<C, B> {
    pub fn new(inner: C) -> Self {
        Self {
            inner,
            _body: PhantomData,
        }
    }
}

impl<C, D, O, B> Service<D> for Endpoint<C, B>
where
    C: Service<D>,
    C::Response: quic::Connection<B::Data, OpenStreams = O> + Send + 'static,
    C::Future: Send + 'static,
    O: quic::OpenStreams<B::Data> + Send + 'static,
    <C::Response as quic::Connection<B::Data>>::SendStream: Send + 'static,
    <C::Response as quic::Connection<B::Data>>::RecvStream: Send + 'static,
    B: Body + Send + 'static,
    B::Data: Send + 'static,
{
    type Response = (
        h3::client::Connection<C::Response, B::Data>,
        super::Connection<O, B>,
    );
    type Error = h3::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: D) -> Self::Future {
        let connecting = self.inner.call(dst);

        Box::pin(async move {
            let conn = connecting
                .await
                .unwrap_or_else(|_| panic!("connecting error"));
            let (driver, send_request) = h3::client::builder().build(conn).await?;
            Ok((driver, super::Connection::new(send_request)))
        })
    }
}
