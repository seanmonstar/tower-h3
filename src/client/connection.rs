use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, Bytes};
use futures::future::Future;
use h3::client::{RequestStream, SendRequest};
use h3::quic;
use http_body::{Body, Frame};
use tokio_util::sync::ReusableBoxFuture;
use tower_service::Service;

pub struct Connection<T, B>
where
    T: quic::OpenStreams<B::Data>,
    B: Body,
{
    tx: SendRequest<T, B::Data>,
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
                fut: ReusableBoxFuture::new(make_fut(stream)),
            };
            Ok(resp.map(move |()| body))
        })
    }
}

pub struct RecvStream<T: quic::RecvStream, B> {
    fut: ReusableBoxFuture<'static, (Option<Result<Frame<Bytes>, h3::Error>>, RequestStream<T, B>)>,
}

impl<T, B> Body for RecvStream<T, B>
where
    T: quic::RecvStream,
    // TODO: bounds
    T: Send + 'static,
    B: Send + 'static,
{
    // TODO: fix buf type
    type Data = Bytes;
    type Error = h3::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let (result, rx) = futures::ready!(self.fut.poll(cx));
        self.fut.set(make_fut(rx));
        Poll::Ready(result)
    }
}

impl<T, B> Future for RecvStream<T, B>
where
    T: quic::RecvStream,
    T: Send + 'static,
    B: Send + 'static,
{
    type Output = Option<Result<Frame<<Self as Body>::Data>, <Self as Body>::Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(futures::ready!(self.poll_frame(cx)))
    }
}

async fn make_fut<T, B>(
    mut rx: RequestStream<T, B>,
) -> (Option<Result<Frame<Bytes>, h3::Error>>, RequestStream<T, B>)
where
    T: quic::RecvStream,
{
    let ret = match rx.recv_data().await {
        Err(e) => Some(Err(e)),

        Ok(Some(mut buf)) => Some(Ok(Frame::data(buf.copy_to_bytes(buf.remaining())))),

        Ok(None) => match rx.recv_trailers().await {
            Err(e) => Some(Err(e)),
            Ok(Some(trailers)) => Some(Ok(Frame::trailers(trailers))),
            Ok(None) => None,
        },
    };

    (ret, rx)
}
