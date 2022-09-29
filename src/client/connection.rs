use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, Bytes};
use futures::future::Future;
use h3::client::{RequestStream, SendRequest};
use h3::quic;
use http_body::Body;
use tokio_util::sync::ReusableBoxFuture;
use tower_service::Service;

pub struct Connection<T, B>
where
    T: quic::OpenStreams<B::Data>,
    B: Body,
{
    tx: SendRequest<T, B::Data>,
}

pub struct RecvStream<T: quic::RecvStream, B> {
    data_fut: ReusableBoxFuture<'static, (Option<Result<Bytes, h3::Error>>, RequestStream<T, B>)>,
}

impl<T, B> Connection<T, B>
where
    T: quic::OpenStreams<B::Data>,
    B: Body,
{
    pub(crate) fn new(tx: SendRequest<T, B::Data>) -> Self {
        Self { tx }
    }
}

impl<T, B> Service<http::Request<B>> for Connection<T, B>
where
    T: quic::OpenStreams<B::Data> + Clone,
    B: Body,

    // TODO: remove bounds, quickly added for `dyn Future`
    T: Send + 'static,
    B: 'static,
    T::BidiStream: Send,
    B::Data: Send,
{
    type Response = http::Response<RecvStream<T::BidiStream, B::Data>>;
    type Error = h3::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let (parts, _body) = req.into_parts();
        let req = http::Request::from_parts(parts, ());

        let mut tx = self.tx.clone();

        Box::pin(async move {
            let mut stream = tx.send_request(req).await?;
            let resp = stream.recv_response().await?;
            let body = RecvStream {
                data_fut: ReusableBoxFuture::new(make_data_fut(stream)),
            };
            Ok(resp.map(move |()| body))
        })
    }
}

impl<T, B> Body for RecvStream<T, B>
where
    T: quic::RecvStream,
    // TODO: bounds
    T: Send + 'static,
    B: Send + 'static,
{
    // TODO: fix buf type
    type Data = bytes::Bytes;
    type Error = h3::Error;

    fn poll_data(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let (result, rx) = futures::ready!(self.data_fut.poll(cx));
        self.data_fut.set(make_data_fut(rx));
        Poll::Ready(result)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        todo!("poll_trailers");
    }
}

async fn make_data_fut<T, B>(
    mut rx: RequestStream<T, B>,
) -> (Option<Result<Bytes, h3::Error>>, RequestStream<T, B>)
where
    T: quic::RecvStream,
{
    let result = rx.recv_data().await;
    let ret = match result {
        Ok(Some(mut buf)) => Some(Ok(buf.copy_to_bytes(buf.remaining()))),
        Ok(None) => None,
        Err(e) => Some(Err(e)),
    };
    (ret, rx)
}
