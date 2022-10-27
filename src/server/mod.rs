use std::collections::VecDeque;
use std::convert::Infallible;
use std::fmt::{Debug, Display, Formatter};
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures::future::{self, Future};
use h3::quic;
use h3::server::RequestStream;
use http::{Request, Response};
use http_body::{Body, Frame};
use tokio_util::sync::ReusableBoxFuture;
use tower::{MakeService, Service};

/// `Server` is responsible for attaching spawned services to connections.
pub struct Server<Svc, C, B>
where
    Svc: MakeService<
        (),
        Request<RecvStream<<C::BidiStream as quic::BidiStream<B::Data>>::RecvStream, B::Data>>,
        Response = Response<B>,
    >,
    C: quic::Connection<B::Data>,
    C::BidiStream: quic::BidiStream<B::Data> + Send + 'static,
    C::RecvStream: quic::RecvStream + Send + 'static,
    B: Body,
    B::Data: Send + 'static,
{
    app: Svc,
    _p: PhantomData<fn(C) -> B>,
}

impl<Svc, C, B> Server<Svc, C, B>
where
    Svc: MakeService<
        (),
        Request<RecvStream<<C::BidiStream as quic::BidiStream<B::Data>>::RecvStream, B::Data>>,
        Response = Response<B>,
    >,
    C: quic::Connection<B::Data>,
    C::BidiStream: quic::BidiStream<B::Data> + Send + 'static,
    C::RecvStream: quic::RecvStream + Send + 'static,
    B: Body,
    B::Data: Send + 'static,
{
    pub fn new(app: Svc) -> Self {
        Server {
            app,
            _p: PhantomData,
        }
    }
}

impl<Svc, C, B> Server<Svc, C, B>
where
    Svc: MakeService<
        (),
        Request<RecvStream<<C::BidiStream as quic::BidiStream<B::Data>>::RecvStream, B::Data>>,
        Response = Response<B>,
    >,
    C: quic::Connection<B::Data>,
    C::BidiStream: quic::BidiStream<B::Data> + Send + 'static,
    <C::BidiStream as quic::BidiStream<B::Data>>::RecvStream: quic::RecvStream + Send + 'static,
    C::RecvStream: quic::RecvStream + Send + 'static,
    B: Body,
    B::Data: Send + 'static,
{
    pub async fn serve(
        &mut self,
        mut conn: h3::server::Connection<C, B::Data>,
    ) -> Result<
        (),
        Error<
            Svc,
            Request<RecvStream<<C::BidiStream as quic::BidiStream<B::Data>>::RecvStream, B::Data>>,
        >,
    > {
        let mut svc = self
            .app
            .make_service(())
            .await
            .map_err(Error::MakeService)?;

        loop {
            match conn.accept().await {
                Ok(Some((req, stream))) => {
                    let (mut send, recv) = stream.split();

                    let (parts, _) = req.into_parts();
                    let body = RecvStream {
                        fut: ReusableBoxFuture::new(make_fut(recv)),
                    };
                    let req = Request::from_parts(parts, body);

                    let resp = svc.call(req).await.map_err(Error::Service)?;
                    let (parts, _) = resp.into_parts();
                    let resp = Response::from_parts(parts, ());

                    send.send_response(resp).await.map_err(Error::Protocol)?;
                    send.finish().await.map_err(Error::Protocol)?;
                }
                Ok(None) => break,
                Err(e) => Err(Error::Protocol(e))?,
            }
        }

        Ok(())
    }
}

// ========== Body ==========

pub struct RecvStream<S, B>
where
    S: quic::RecvStream,
    B: Buf,
{
    fut: ReusableBoxFuture<'static, (Option<Result<Frame<Bytes>, h3::Error>>, RequestStream<S, B>)>,
}

impl<S, B> Body for RecvStream<S, B>
where
    S: quic::RecvStream + Send + 'static,
    B: Buf + Send + 'static,
{
    type Data = Bytes;
    type Error = h3::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let (res, rx) = futures::ready!(self.fut.poll(cx));
        self.fut.set(make_fut(rx));
        Poll::Ready(res)
    }
}

impl<S, B> Future for RecvStream<S, B>
where
    S: quic::RecvStream + Send + 'static,
    B: Buf + Send + 'static,
{
    type Output = Result<Bytes, h3::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut bufs = VecDeque::new();

        loop {
            match futures::ready!(Pin::new(&mut self).poll_frame(cx)) {
                None => break,
                Some(Err(e)) => return Poll::Ready(Err(e)),
                Some(Ok(frame)) => {
                    if frame.is_data() {
                        bufs.push_back(frame.into_data().unwrap());
                    }
                }
            }
        }

        let cap = bufs.iter().map(|b| b.len()).sum();
        let mut bytes = BytesMut::with_capacity(cap);

        while let Some(b) = bufs.pop_front() {
            bytes.put(b);
        }

        // TODO return (data, trailers)
        Poll::Ready(Ok(bytes.freeze()))
    }
}

async fn make_fut<S: quic::RecvStream, B: Buf>(
    mut rx: RequestStream<S, B>,
) -> (Option<Result<Frame<Bytes>, h3::Error>>, RequestStream<S, B>) {
    let ret = match rx.recv_data().await {
        Err(e) => Some(Err(e)),
        Ok(Some(mut buf)) => Some(Ok(Frame::data(buf.copy_to_bytes(buf.remaining())))),
        Ok(None) => None,
        // TODO
        // Ok(None) => match rx.recv_trailers().await {
        //     Err(e) => Some(Err(e)),
        //     Ok(Some(trailers)) => Some(Ok(Frame::trailers(trailers))),
        //     Ok(None) => None,
        // },
    };

    // TODO should return generic Buf instead of Bytes
    // once we have the typing in h3 sorted out
    (ret, rx)
}

// ========== Error ==========

pub enum Error<Svc, R>
where
    Svc: MakeService<(), R>,
{
    Protocol(h3::Error),
    MakeService(Svc::MakeError),
    Service(Svc::Error),
}

impl<Svc, R> Debug for Error<Svc, R>
where
    Svc: MakeService<(), R>,
    Svc::MakeError: Debug,
    Svc::Error: Debug,
{
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        match self {
            Error::Protocol(ref err) => f.debug_tuple("Protocol").field(err).finish(),
            Error::MakeService(ref err) => f.debug_tuple("MakeService").field(err).finish(),
            Error::Service(ref err) => f.debug_tuple("Service").field(err).finish(),
        }
    }
}

impl<Svc, R> Display for Error<Svc, R>
where
    Svc: MakeService<(), R>,
    Svc::MakeError: Display,
    Svc::Error: Display,
{
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        match self {
            Error::Protocol(ref err) => write!(f, "HTTP/3: {err}"),
            Error::MakeService(ref err) => write!(f, "MakService: {err}"),
            Error::Service(ref err) => write!(f, "App: {err}"),
        }
    }
}

impl<Svc, R> std::error::Error for Error<Svc, R>
where
    Svc: MakeService<(), R>,
    Svc::MakeError: std::error::Error,
    Svc::Error: std::error::Error,
{
}

// ========== MakeService ==========

/// `App` is used to create `MakeService` that spawns instances of the
/// user provided `Service`, provided for convenience.
#[derive(Clone, Copy)]
pub struct App<S> {
    svc: S,
}

impl<S> App<S> {
    pub fn new(svc: S) -> Self {
        App { svc }
    }
}

impl<S> Service<()> for App<S>
where
    S: Clone + Send + 'static,
{
    type Response = S;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: ()) -> Self::Future {
        Box::pin(future::ok(self.svc.clone()))
    }
}
