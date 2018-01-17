use std::cell::RefCell;
use std::fmt::{self, Write};
use std::io;
use std::mem;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use futures::{Async, Future, Poll, Stream};
use futures::future::{self, Executor};
use h2;
use http;
use http::header::{HeaderValue, HOST};
use http::uri::{Authority, Parts, Scheme, Uri};
use httparse;
use hyper;
use tokio_core::reactor::Handle;
use tokio_connect::Connect;
use tokio_io::AsyncRead;
use tokio_io::io::copy;
use tower::{NewService, Service};
use tower_h2;

use bind;
use connection::Connection;
use control;
use ctx::Proxy as ProxyCtx;
use ctx::transport::{Client as ClientCtx, Server as ServerCtx};
use telemetry::Sensors;
use timeout::Timeout;
use transport::{self, GetOriginalDst};

pub struct Server<S: NewService, E, B: tower_h2::Body, G>
where
    S: NewService<Request=http::Request<HttpBody>>,
    S::Future: 'static,
{
    executor: E,
    get_orig_dst: G,
    h1: hyper::server::Http,
    h2: tower_h2::Server<HttpBodyNewService<S>, E, B>,
    listen_addr: SocketAddr,
    new_service: S,
    proxy_ctx: Arc<ProxyCtx>,
    sensors: Sensors,
}

pub enum Client<C, B>
where
    B: tower_h2::Body,
{
    Http1(Arc<hyper::Client<HyperConnect<C>, BodyStream<B>>>),
    //TODO: logging::context_executor
    Http2(tower_h2::client::Client<C, Handle, B>),
}

pub enum ClientService<C, B>
where
    B: tower_h2::Body,
{
    Http1(Arc<hyper::Client<HyperConnect<C>, BodyStream<B>>>),
    Http2(tower_h2::client::Service<C, Handle, B>),
}

pub enum HttpBody {
    Http1(hyper::Body),
    Http2(tower_h2::RecvBody),
}

// ===== impl Server =====

impl<S, /*E,*/ B, G> Server<S, Handle/*E*/, B, G>
where
    S: NewService<
        Request = http::Request<HttpBody>,
        Response = http::Response<B>
    > + Clone + 'static,
    S::Future: 'static,
    S::Error: fmt::Debug,

    //E: Executor<tower_h2::server::Background<<S::Service as Service>::Future, B>>,
    //E: Executor<Box<Future<Item=(), Error=()>>>,
    //E: Clone + 'static,
    B: tower_h2::Body + 'static,
    G: GetOriginalDst,
{
    pub fn new(
        listen_addr: SocketAddr,
        proxy_ctx: Arc<ProxyCtx>,
        sensors: Sensors,
        get_orig_dst: G,
        stack: S,
        executor: Handle,
    ) -> Self {
        let recv_body_svc = HttpBodyNewService {
            new_service: stack.clone(),
        };
        Server {
            executor: executor.clone(),
            get_orig_dst,
            h1: hyper::server::Http::new(),
            h2: tower_h2::Server::new(recv_body_svc, Default::default(), executor),
            listen_addr,
            new_service: stack,
            proxy_ctx,
            sensors,
        }
    }

    pub fn serve(&self, connection: Connection, remote_addr: SocketAddr) {
        let opened_at = Instant::now();

        // create Server context
        let orig_dst = connection.original_dst_addr(&self.get_orig_dst);
        let local_addr = connection.local_addr().unwrap_or(self.listen_addr);
        let proxy_ctx = self.proxy_ctx.clone();

        // try to sniff protocol
        let sniff = [0u8; 32];
        let sensors = self.sensors.clone();
        let h1 = self.h1.clone();
        let h2 = self.h2.clone();
        let new_service = self.new_service.clone();
        let handle = self.executor.clone();
        let fut = connection
            .peek_future(sniff)
            .map_err(|_| ())
            .and_then(move |(connection, sniff, n)| -> Box<Future<Item=(), Error=()>> {
                match Protocol::detect(&sniff[..n]) {
                    Protocol::Tcp => {
                        trace!("transparency sniffed TCP");

                        let srv_ctx = ServerCtx::new(
                            &proxy_ctx,
                            &local_addr,
                            &remote_addr,
                            &orig_dst,
                            control::pb::proxy::common::Protocol::Tcp,
                        );

                        // record telemetry
                        let tcp_in = sensors.accept(connection, opened_at, &srv_ctx);

                        // For TCP, we really have no extra information other than the
                        // SO_ORIGINAL_DST socket option. If that isn't set, the only thing
                        // to do is to drop this connection.
                        let orig_dst = if let Some(orig_dst) = orig_dst {
                            debug!(
                                "tcp accepted, forwarding ({}) to {}",
                                remote_addr,
                                orig_dst,
                            );
                            orig_dst
                        } else {
                            debug!(
                                "tcp accepted, no SO_ORIGINAL_DST to forward: remote={}",
                                remote_addr,
                            );
                            return Box::new(future::ok(()));
                        };

                        let client_ctx = ClientCtx::new(
                            &srv_ctx.proxy,
                            &orig_dst,
                            control::pb::proxy::common::Protocol::Tcp,
                        );
                        let c = Timeout::new(
                            transport::Connect::new(orig_dst, &handle),
                            ::std::time::Duration::from_millis(300),
                            &handle,
                        );

                        let fut = sensors.connect(c, &client_ctx)
                            .connect()
                            .map_err(|e| debug!("tcp connect error: {:?}", e))
                            .and_then(move |tcp_out| {
                                let (in_r, in_w) = tcp_in.split();
                                let (out_r, out_w) = tcp_out.split();

                                copy(in_r, out_w)
                                    .join(copy(out_r, in_w))
                                    .map(|_| ())
                                    .map_err(|e| debug!("tcp error: {}", e))
                            });
                        Box::new(fut)
                    },
                    Protocol::Http1 => {
                        trace!("transparency sniffed HTTP/1");

                        let srv_ctx = ServerCtx::new(
                            &proxy_ctx,
                            &local_addr,
                            &remote_addr,
                            &orig_dst,
                            control::pb::proxy::common::Protocol::Http,
                        );

                        // record telemetry
                        let io = sensors.accept(connection, opened_at, &srv_ctx);
                        Box::new(new_service.new_service()
                            .map_err(|_| ())
                            .and_then(move |s| {
                                let svc = HyperServerSvc {
                                    service: RefCell::new(s),
                                    srv_ctx,
                                };
                                h1.serve_connection(io, svc)
                                    .map(|_| ())
                                    .map_err(|_| ())
                            }))
                    },
                    Protocol::Http2 => {
                        trace!("transparency sniffed HTTP/2");

                        let srv_ctx = ServerCtx::new(
                            &proxy_ctx,
                            &local_addr,
                            &remote_addr,
                            &orig_dst,
                            control::pb::proxy::common::Protocol::Http,
                        );

                        // record telemetry
                        let io = sensors.accept(connection, opened_at, &srv_ctx);

                        // handle http
                        let set_ctx = move |request: &mut http::Request<()>| {
                            request.extensions_mut().insert(srv_ctx.clone());
                        };
                        Box::new(h2.serve_modified(io, set_ctx).map_err(|_| ()))
                    }
                }
            });

        self.executor.execute(Box::new(fut) as Box<Future<Item=(), Error=()>>).unwrap();
    }
}

// ===== impl Client =====

impl<C, B> Client<C, B>
where
    C: Connect + 'static,
    C::Future: 'static,
    B: tower_h2::Body + 'static,
{
    pub fn new(protocol: bind::Protocol, connect: C, executor: Handle) -> Self {
        match protocol {
            bind::Protocol::Http1 => {
                let h1 = hyper::Client::configure()
                    .connector(HyperConnect {
                        connect,
                    })
                    .body()
                    .build(&executor);
                Client::Http1(Arc::new(h1))
            },
            bind::Protocol::Http2 => {
                let mut h2_builder = h2::client::Builder::default();
                // h2 currently doesn't handle PUSH_PROMISE that well, so we just
                // disable it for now.
                h2_builder.enable_push(false);
                let h2 = tower_h2::client::Client::new(connect, h2_builder, executor);

                Client::Http2(h2)
            }
        }
    }
}

impl<C, B> NewService for Client<C, B>
where
    C: Connect + 'static,
    C::Future: 'static,
    B: tower_h2::Body + 'static,
{
    type Request = http::Request<B>;
    type Response = http::Response<HttpBody>;
    type Error = tower_h2::client::Error;
    type InitError = tower_h2::client::ConnectError<C::Error>;
    type Service = ClientService<C, B>;
    type Future = Box<Future<Item=Self::Service, Error=Self::InitError>>;

    fn new_service(&self) -> Self::Future {
        match *self {
            Client::Http1(ref h1) => {
                Box::new(future::ok(ClientService::Http1(h1.clone())))
            },
            Client::Http2(ref h2) => {
                Box::new(h2.new_service().map(|s| ClientService::Http2(s)))
            }
        }
    }
}

impl<C, B> Service for ClientService<C, B>
where
    C: Connect + 'static,
    C::Future: 'static,
    B: tower_h2::Body + 'static,
{
    type Request = http::Request<B>;
    type Response = http::Response<HttpBody>;
    type Error = tower_h2::client::Error;
    type Future = ClientServiceFuture;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        match *self {
            ClientService::Http1(_) => Ok(Async::Ready(())),
            ClientService::Http2(ref mut h2) => h2.poll_ready(),
        }
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        match *self {
            ClientService::Http1(ref h1) => {
                trace!("client h1 req; uri={:?}", req.uri());
                let req: hyper::Request<BodyStream<B>> = req.map(BodyStream).into();
                ClientServiceFuture::Http1(h1.request(req))
            },
            ClientService::Http2(ref mut h2) => {
                ClientServiceFuture::Http2(h2.call(req))
            },
        }
    }
}

pub enum ClientServiceFuture {
    Http1(hyper::client::FutureResponse),
    Http2(tower_h2::client::ResponseFuture),
}

impl Future for ClientServiceFuture {
    type Item = http::Response<HttpBody>;
    type Error = tower_h2::client::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match *self {
            ClientServiceFuture::Http1(ref mut f) => {
                match f.poll() {
                    Ok(Async::Ready(res)) => {
                        let res = http::Response::from(res);
                        let res = res.map(HttpBody::Http1);
                        Ok(Async::Ready(res))
                    },
                    Ok(Async::NotReady) => Ok(Async::NotReady),
                    Err(e) => {
                        debug!("http/1 client error: {}", e);
                        Err(h2::Reason::INTERNAL_ERROR.into())
                    }
                }
            },
            ClientServiceFuture::Http2(ref mut f) => {
                let res = try_ready!(f.poll());
                let res = res.map(HttpBody::Http2);
                Ok(Async::Ready(res))
            }
        }
    }
}

// ===== impl Protocol =====

#[derive(Debug)]
enum Protocol {
    Tcp,
    Http1,
    Http2,
}

const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

impl Protocol {
    fn detect(bytes: &[u8]) -> Protocol {
        // http2 is easiest to detect
        if bytes.len() >= H2_PREFACE.len() {
            if &bytes[..H2_PREFACE.len()] == H2_PREFACE {
                return Protocol::Http2;
            }
        }

        // http1 can have a really long first line, but if the bytes so far
        // look like http1, we'll assume it is. a different protocol
        // should look different in the first few bytes

        let mut headers = [httparse::EMPTY_HEADER; 0];
        let mut req = httparse::Request::new(&mut headers);
        match req.parse(bytes) {
            // Ok(Compelete) or Ok(Partial) both mean it looks like HTTP1!
            //
            // If we got past the first line, we'll see TooManyHeaders,
            // because we passed an array of 0 headers to parse into. That's fine!
            // We didn't want to keep parsing headers, just validate that
            // the first line is HTTP1.
            Ok(_) | Err(httparse::Error::TooManyHeaders) => {
                return Protocol::Http1;
            },
            _ => {}
        }

        Protocol::Tcp
    }
}

// ===== impl HttpBody =====

impl tower_h2::Body for HttpBody {
    type Data = Bytes;

    fn is_end_stream(&self) -> bool {
        match *self {
            HttpBody::Http1(_) => false,
            HttpBody::Http2(ref b) => b.is_end_stream(),
        }
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, h2::Error> {
        match *self {
            HttpBody::Http1(ref mut b) => {
                match b.poll() {
                    Ok(Async::Ready(Some(chunk))) => Ok(Async::Ready(Some(chunk.into()))),
                    Ok(Async::Ready(None)) => Ok(Async::Ready(None)),
                    Ok(Async::NotReady) => Ok(Async::NotReady),
                    Err(e) => {
                        debug!("http/1 body error: {}", e);
                        Err(h2::Reason::INTERNAL_ERROR.into())
                    }
                }
            },
            HttpBody::Http2(ref mut b) => b.poll_data().map(|async| async.map(|opt| opt.map(|data| data.into()))),
        }
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, h2::Error> {
        match *self {
            HttpBody::Http1(_) => Ok(Async::Ready(None)),
            HttpBody::Http2(ref mut b) => b.poll_trailers(),
        }
    }
}

impl Default for HttpBody {
    fn default() -> HttpBody {
        HttpBody::Http2(Default::default())
    }
}

#[derive(Debug)]
pub struct BodyStream<B>(B);

impl<B> Stream for BodyStream<B>
where
    B: tower_h2::Body,
{
    type Item = BufAsRef<<B::Data as ::bytes::IntoBuf>::Buf>;
    type Error = hyper::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.0.poll_data()
            .map(|async| async.map(|opt| opt.map(|buf| BufAsRef(::bytes::IntoBuf::into_buf(buf)))))
            .map_err(|e| {
                trace!("h2 body error: {:?}", e);
                hyper::Error::Io(io::ErrorKind::Other.into())
            })
    }
}

#[derive(Debug)]
pub struct BufAsRef<B>(B);

impl<B: ::bytes::Buf> AsRef<[u8]> for BufAsRef<B> {
    fn as_ref(&self) -> &[u8] {
        ::bytes::Buf::bytes(&self.0)
    }
}


// ===== impl HyperServerSvc =====

struct HyperServerSvc<S> {
    service: RefCell<S>,
    srv_ctx: Arc<ServerCtx>,
}

impl<S, B> hyper::server::Service for HyperServerSvc<S>
where
    S: Service<
        Request=http::Request<HttpBody>,
        Response=http::Response<B>,
    >,
    S::Error: fmt::Debug,
    S::Future: 'static,
    B: tower_h2::Body + 'static,
{
    type Request = hyper::server::Request;
    type Response = hyper::server::Response<BodyStream<B>>;
    type Error = hyper::Error;
    //TODO: unbox me
    type Future = Box<Future<Item=Self::Response, Error=Self::Error>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let mut req: http::Request<hyper::Body> = req.into();
        req.extensions_mut().insert(self.srv_ctx.clone());

        if let Err(()) = reconstruct_uri(&mut req) {
            let res = hyper::Response::new()
                .with_status(hyper::BadRequest);
            return Box::new(future::ok(res));
        }
        strip_connection_headers(req.headers_mut());

        let req = req.map(|b| HttpBody::Http1(b));
        let f = self.service.borrow_mut().call(req)
            .map(|mut res| {
                strip_connection_headers(res.headers_mut());
                res.map(|b| BodyStream(b)).into()
            })
            .map_err(|e| {
                debug!("h2 error: {:?}", e);
                hyper::Error::Io(io::ErrorKind::Other.into())
            });
        Box::new(f)
    }
}

fn reconstruct_uri<B>(req: &mut http::Request<B>) -> Result<(), ()> {
    // RFC7230#section-5.4
    // If an absolute-form uri is received, it must replace
    // the host header
    if let Some(auth) = req.uri().authority_part().cloned() {
        if let Some(host) = req.headers().get(HOST) {
            if auth.as_str().as_bytes() == host.as_bytes() {
                // host and absolute-form agree, nothing more to do
                return Ok(());
            }
        }
        let host = HeaderValue::from_shared(auth.into_bytes())
            .expect("a valid authority is valid header value");
        req.headers_mut().insert(HOST, host);
        return Ok(());
    }

    // try to parse the Host header
    if let Some(host) = req.headers().get(HOST).cloned() {
        let auth = host.to_str()
            .ok()
            .and_then(|s| {
                if s.is_empty() {
                    None
                } else {
                    s.parse::<Authority>().ok()
                }
            });
        if let Some(auth) = auth {
            set_authority(req.uri_mut(), auth);
            return Ok(());
        }
    }

    // last resort is to use the so_original_dst
    let orig_dst = req.extensions()
        .get::<Arc<ServerCtx>>()
        .and_then(|ctx| ctx.orig_dst_if_not_local());
    if let Some(orig_dst) = orig_dst {
        let mut bytes = BytesMut::with_capacity(31);
        write!(&mut bytes, "{}", orig_dst)
            .expect("socket address display is under 31 bytes");
        let bytes = bytes.freeze();
        let auth = Authority::from_shared(bytes)
            .expect("socket address is valid authority");
        set_authority(req.uri_mut(), auth);

        return Ok(());
    }

    Err(())
}

fn set_authority(uri: &mut http::Uri, auth: Authority) {
    let mut parts = Parts::from(mem::replace(uri, Uri::default()));
    parts.scheme = Some(Scheme::HTTP);
    parts.authority = Some(auth);

    let new = Uri::from_parts(parts)
        .expect("absolute uri");

    *uri = new;
}

fn strip_connection_headers(headers: &mut http::HeaderMap) {
    let conn_val = if let Some(val) = headers.remove(http::header::CONNECTION) {
        val
    } else {
        return
    };

    let conn_header = if let Ok(s) = conn_val.to_str() {
        s
    } else {
        return
    };

    // A `Connection` header may have a comma-separated list of
    // names of other headers that are meant for only this specific connection.
    //
    // Iterate these names and remove them as headers.
    for name in conn_header.split(',') {
        let name = name.trim();
        headers.remove(name);
    }
}


// ==== impl HttpBodyService ====


struct HttpBodyService<S> {
    service: S,
}

impl<S> Service for HttpBodyService<S>
where
    S: Service<
        Request=http::Request<HttpBody>,
    >,
{
    type Request = http::Request<tower_h2::RecvBody>;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready()
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        self.service.call(req.map(|b| HttpBody::Http2(b)))
    }
}

#[derive(Clone)]
struct HttpBodyNewService<N> {
    new_service: N,
}

impl<N> NewService for HttpBodyNewService<N>
where
    N: NewService<Request=http::Request<HttpBody>>,
    N::Future: 'static,
{
    type Request = http::Request<tower_h2::RecvBody>;
    type Response = N::Response;
    type Error = N::Error;
    type Service = HttpBodyService<N::Service>;
    type InitError = N::InitError;
    type Future = Box<Future<Item=Self::Service, Error=Self::InitError>>;

    fn new_service(&self) -> Self::Future {
        Box::new(self.new_service.new_service().map(|s| {
            HttpBodyService {
                service: s,
            }
        }))
    }
}

// ===== impl HyperConnect =====

pub struct HyperConnect<C> {
    connect: C,
}

impl<C> hyper::client::Service for HyperConnect<C>
where
    C: Connect,
    C::Future: 'static,
{
    type Request = hyper::Uri;
    type Response = C::Connected;
    type Error = io::Error;
    type Future = Box<Future<Item=Self::Response, Error=Self::Error>>;

    fn call(&self, _uri: Self::Request) -> Self::Future {
        Box::new(self.connect.connect()
            .map_err(|_e| io::Error::new(io::ErrorKind::Other, "blah")))
    }
}
