use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{self, Poll};

use tower_service::Service;

pub struct Endpoint<C, B> {
    inner: C,
    _body: PhantomData<fn() -> fn(B)>,
}

impl<C, Dst, O, B> Service<Dst> for Endpoint<C, B>
where
    C: Service<Dst>,
    C::Response: h3::quic::Connection<B::Data, OpenStreams = O> + Send + 'static,
    C::Future: Send + 'static,
    O: h3::quic::OpenStreams<B::Data> + Send + 'static,
    <C::Response as h3::quic::Connection<B::Data>>::SendStream: Send + 'static,
    B: http_body::Body + Send + 'static,
    B::Data: Send + 'static,
{
    type Response = super::Connection<O, B>;
    type Error = h3::Error;
    type Future = Pin<Box<dyn std::future::Future<Output=Result<Self::Response, Self::Error>> + Send>>;


    fn poll_ready(&mut self, _cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: Dst) -> Self::Future {
        let connecting = self.inner.call(dst);

        Box::pin(async move {
            let conn = connecting.await.unwrap_or_else(|_| panic!("connecting error"));

            let (driver, tx) = h3::client::builder()
                .build(conn)
                .await?;
            Ok(super::Connection::new(tx))
        })
    }
}
