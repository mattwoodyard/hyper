//! HTTP Server
//!
//! A `Server` is created to listen on a port, parse HTTP requests, and hand
//! them off to a `Service`.

use std::cell::RefCell;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::rc::{Rc, Weak};
use std::time::Duration;

use futures::future;
use futures::task::{self, Task};
use futures::{Future, Map, Stream, Poll, Async, Sink, StartSend, AsyncSink};

use tokio::io::Io;
use tokio::reactor::{Core, Handle, Timeout};
use tokio::net::{TcpListener, TcpStream};
use tokio_proto::BindServer;
use tokio_proto::streaming::Message;
use tokio_proto::streaming::pipeline::{Transport, Frame, ServerProto};
pub use tokio_service::{NewService, Service};

pub use self::request::Request;
pub use self::response::Response;

use http;

mod request;
mod response;

/// An instance of the HTTP protocol, and implementation of tokio-proto's
/// `ServerProto` trait.
///
/// This structure is used to create instances of `Server` or to spawn off tasks
/// which handle a connection to an HTTP server. Each instance of `Http` can be
/// configured with various protocol-level options such as keepalive.
#[derive(Debug, Clone)]
pub struct Http {
    keep_alive: bool,
}


type ConnectionStream<I: Io> = Box<Stream<Item=(Box<I>, SocketAddr), Error=io::Error>>;

/// An instance of a server created through `Http::bind`.
///
/// This server is intended as a convenience for creating a TCP listener on an
/// address and then serving TCP connections accepted with the service provided.
pub struct Server<S, I: Io + 'static> {
    protocol: Http,
    new_service: S,
    core: Core,
    local_address: SocketAddr,
    incoming: ConnectionStream<I>,
    shutdown_timeout: Duration,
}

impl Http {
    /// Creates a new instance of the HTTP protocol, ready to spawn a server or
    /// start accepting connections.
    pub fn new() -> Http {
        Http {
            keep_alive: true,
        }
    }

    /// Enables or disables HTTP keep-alive.
    ///
    /// Default is true.
    pub fn keep_alive(&mut self, val: bool) -> &mut Self {
        self.keep_alive = val;
        self
    }

    /// Bind the provided `addr` and return a server ready to handle
    /// connections.
    ///
    /// This method will bind the `addr` provided with a new TCP listener ready
    /// to accept connections. Each connection will be processed with the
    /// `new_service` object provided as well, creating a new service per
    /// connection.
    ///
    /// The returned `Server` contains one method, `run`, which is used to
    /// actually run the server.
    pub fn bind<S>(&self, addr: &SocketAddr, new_service: S) -> ::Result<Server<S, TcpStream>>
        where S: NewService<Request = Request, Response = Response, Error = ::Error> +
                    Send + Sync + 'static,
    {
        let core = try!(Core::new());
        let handle = core.handle();
        let listener = try!(TcpListener::bind(addr, &handle));
        let local_addr = try!(listener.local_addr());

        Ok(Server {
            new_service: new_service,
            core: core,
            local_address: local_addr,
            incoming: Box::new(listener.incoming().map(|(s,a)| { (Box::new(s), a) })),
            protocol: self.clone(),
            shutdown_timeout: Duration::new(1, 0),
        })
    }
    /// Bind the provided incoming stream and return a server ready to handle
    /// connections.
    ///
    ///
    /// This method will setup a Server on the provided Core and ConnectionStream 
    ///
    /// to accept connections. Each connection will be processed with the
    /// `new_service` object provided as well, creating a new service per
    /// connection.
    ///
    /// 
    /// The returned `Server` contains one method, `run`, which is used to
    /// actually run the server.
    pub fn bind_existing<S, I>(&self, local_addr: SocketAddr, core: Core, listener: ConnectionStream<I>, new_service: S) -> ::Result<Server<S, I>>
    where S: NewService<Request = Request, Response = Response, Error = ::Error> + Send + Sync + 'static,
          I: Io + 'static
    {

    Ok(Server {
        new_service: new_service,
        core: core,
        local_address: local_addr,
        incoming: listener,
        protocol: self.clone(),
        shutdown_timeout: Duration::new(1, 0),
    })
}





    /// Use this `Http` instance to create a new server task which handles the
    /// connection `io` provided.
    ///
    /// This is the low-level method used to actually spawn handling a TCP
    /// connection, typically. The `handle` provided is the event loop on which
    /// the server task will be spawned, `io` is the I/O object associated with
    /// this connection (data that's read/written), `remote_addr` is the remote
    /// peer address of the HTTP client, and `service` defines how HTTP requests
    /// will be handled (and mapped to responses).
    ///
    /// This method is typically not invoked directly but is rather transitively
    /// used through the `serve` helper method above. This can be useful,
    /// however, when writing mocks or accepting sockets from a non-TCP
    /// location.
    pub fn bind_connection<S, I>(&self,
                                 handle: &Handle,
                                 io: I,
                                 remote_addr: SocketAddr,
                                 service: S)
        where S: Service<Request = Request, Response = Response, Error = ::Error> + 'static,
              I: Io + 'static,
    {
        self.bind_server(handle, io, HttpService {
            inner: service,
            remote_addr: remote_addr,
        })
    }
}

#[doc(hidden)]
#[allow(missing_debug_implementations)]
pub struct ProtoRequest(http::RequestHead);
#[doc(hidden)]
#[allow(missing_debug_implementations)]
pub struct ProtoResponse(ResponseHead);
#[doc(hidden)]
#[allow(missing_debug_implementations)]
pub struct ProtoTransport<T>(http::Conn<T, http::ServerTransaction>);
#[doc(hidden)]
#[allow(missing_debug_implementations)]
pub struct ProtoBindTransport<T> {
    inner: future::FutureResult<http::Conn<T, http::ServerTransaction>, io::Error>,
}

impl<T: Io + 'static> ServerProto<T> for Http {
    type Request = ProtoRequest;
    type RequestBody = http::Chunk;
    type Response = ProtoResponse;
    type ResponseBody = http::Chunk;
    type Error = ::Error;
    type Transport = ProtoTransport<T>;
    type BindTransport = ProtoBindTransport<T>;

    fn bind_transport(&self, io: T) -> Self::BindTransport {
        let ka = if self.keep_alive {
            http::KA::Busy
        } else {
            http::KA::Disabled
        };
        ProtoBindTransport {
            inner: future::ok(http::Conn::new(io, ka)),
        }
    }
}

impl<T: Io + 'static> Sink for ProtoTransport<T> {
    type SinkItem = Frame<ProtoResponse, http::Chunk, ::Error>;
    type SinkError = io::Error;

    fn start_send(&mut self, item: Self::SinkItem)
                  -> StartSend<Self::SinkItem, io::Error> {
        let item = match item {
            Frame::Message { message, body } => {
                Frame::Message { message: message.0, body: body }
            }
            Frame::Body { chunk } => Frame::Body { chunk: chunk },
            Frame::Error { error } => Frame::Error { error: error },
        };
        match try!(self.0.start_send(item)) {
            AsyncSink::Ready => Ok(AsyncSink::Ready),
            AsyncSink::NotReady(Frame::Message { message, body }) => {
                Ok(AsyncSink::NotReady(Frame::Message {
                    message: ProtoResponse(message),
                    body: body,
                }))
            }
            AsyncSink::NotReady(Frame::Body { chunk }) => {
                Ok(AsyncSink::NotReady(Frame::Body { chunk: chunk }))
            }
            AsyncSink::NotReady(Frame::Error { error }) => {
                Ok(AsyncSink::NotReady(Frame::Error { error: error }))
            }
        }
    }

    fn poll_complete(&mut self) -> Poll<(), io::Error> {
        self.0.poll_complete()
    }
}

impl<T: Io + 'static> Stream for ProtoTransport<T> {
    type Item = Frame<ProtoRequest, http::Chunk, ::Error>;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, io::Error> {
        let item = match try_ready!(self.0.poll()) {
            Some(item) => item,
            None => return Ok(None.into()),
        };
        let item = match item {
            Frame::Message { message, body } => {
                Frame::Message { message: ProtoRequest(message), body: body }
            }
            Frame::Body { chunk } => Frame::Body { chunk: chunk },
            Frame::Error { error } => Frame::Error { error: error },
        };
        Ok(Some(item).into())
    }
}

impl<T: Io + 'static> Transport for ProtoTransport<T> {
    fn tick(&mut self) {
        self.0.tick()
    }

    fn cancel(&mut self) -> io::Result<()> {
        self.0.cancel()
    }
}

impl<T: Io + 'static> Future for ProtoBindTransport<T> {
    type Item = ProtoTransport<T>;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<ProtoTransport<T>, io::Error> {
        self.inner.poll().map(|a| a.map(ProtoTransport))
    }
}

struct HttpService<T> {
    inner: T,
    remote_addr: SocketAddr,
}

fn map_response_to_message(res: Response) -> Message<ProtoResponse, http::TokioBody> {
    let (head, body) = response::split(res);
    if let Some(body) = body {
        Message::WithBody(ProtoResponse(head), body.into())
    } else {
        Message::WithoutBody(ProtoResponse(head))
    }
}

type ResponseHead = http::MessageHead<::StatusCode>;

impl<T> Service for HttpService<T>
    where T: Service<Request=Request, Response=Response, Error=::Error>,
{
    type Request = Message<ProtoRequest, http::TokioBody>;
    type Response = Message<ProtoResponse, http::TokioBody>;
    type Error = ::Error;
    type Future = Map<T::Future, fn(Response) -> Message<ProtoResponse, http::TokioBody>>;

    fn call(&self, message: Self::Request) -> Self::Future {
        let (head, body) = match message {
            Message::WithoutBody(head) => (head.0, http::Body::empty()),
            Message::WithBody(head, body) => (head.0, body.into()),
        };
        let req = request::new(self.remote_addr, head, body);
        self.inner.call(req).map(map_response_to_message)
    }
}

impl<S, I> Server<S, I>
    where S: NewService<Request = Request, Response = Response, Error = ::Error> + Send + Sync + 'static,
          I: Io + 'static
{
    /// Returns the local address that this server is bound to.
    pub fn local_addr(&self) -> ::Result<SocketAddr> {
        Ok(self.local_address)
    }

    /// Returns a handle to the underlying event loop that this server will be
    /// running on.
    pub fn handle(&self) -> Handle {
        self.core.handle()
    }

    /// Configure the amount of time this server will wait for a "graceful
    /// shutdown".
    ///
    /// This is the amount of time after the shutdown signal is received the
    /// server will wait for all pending connections to finish. If the timeout
    /// elapses then the server will be forcibly shut down.
    ///
    /// This defaults to 1s.
    pub fn shutdown_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.shutdown_timeout = timeout;
        self
    }

    /// Execute this server infinitely.
    ///
    /// This method does not currently return, but it will return an error if
    /// one occurs.
    pub fn run(self) -> ::Result<()> {
        self.run_until(future::empty())
    }

    /// Execute this server until the given future, `shutdown_signal`, resolves.
    ///
    /// This method, like `run` above, is used to execute this HTTP server. The
    /// difference with `run`, however, is that this method allows for shutdown
    /// in a graceful fashion. The future provided is interpreted as a signal to
    /// shut down the server when it resolves.
    ///
    /// This method will block the current thread executing the HTTP server.
    /// When the `shutdown_signal` has resolved then the TCP listener will be
    /// unbound (dropped). The thread will continue to block for a maximum of
    /// `shutdown_timeout` time waiting for active connections to shut down.
    /// Once the `shutdown_timeout` elapses or all active connections are
    /// cleaned out then this method will return.
    pub fn run_until<F>(self, shutdown_signal: F) -> ::Result<()>
        where F: Future<Item = (), Error = ::Error>,
    {
        let Server { protocol, new_service, mut core, local_address, incoming, shutdown_timeout } = self;
        let handle = core.handle();

        // Unused :)
        { let _ =local_address; }

        // Mini future to track the number of active services
        let info = Rc::new(RefCell::new(Info {
            active: 0,
            blocker: None,
        }));

        // Future for our server's execution
        let srv = incoming.for_each(|(socket, addr)| {
            let s = NotifyService {
                inner: try!(new_service.new_service()),
                info: Rc::downgrade(&info),
            };
            info.borrow_mut().active += 1;
            protocol.bind_connection(&handle, *socket, addr, s);
            Ok(())
        });

        // Main execution of the server. Here we use `select` to wait for either
        // `incoming` or `f` to resolve. We know that `incoming` will never
        // resolve with a success (it's infinite) so we're actually just waiting
        // for an error or for `f`, our shutdown signal.
        //
        // When we get a shutdown signal (`Ok`) then we drop the TCP listener to
        // stop accepting incoming connections.
        match core.run(shutdown_signal.select(srv.map_err(|e| e.into()))) {
            Ok(((), _incoming)) => {}
            Err((e, _other)) => return Err(e),
        }

        // Ok we've stopped accepting new connections at this point, but we want
        // to give existing connections a chance to clear themselves out. Wait
        // at most `shutdown_timeout` time before we just return clearing
        // everything out.
        //
        // Our custom `WaitUntilZero` will resolve once all services constructed
        // here have been destroyed.

        let timeout = try!(Timeout::new(shutdown_timeout, &handle));
        let wait = WaitUntilZero { info: info.clone() };
        match core.run(wait.select(timeout)) {
            Ok(_) => Ok(()),
            Err((e, _)) => return Err(e.into())
        }
    }
}

impl<S: fmt::Debug, I: Io> fmt::Debug for Server<S, I> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Server")
         .field("core", &"...")
         .field("local_address", &self.local_address)
         .field("new_service", &self.new_service)
         .field("protocol", &self.protocol)
         .finish()
    }
}

struct NotifyService<S> {
    inner: S,
    info: Weak<RefCell<Info>>,
}

struct WaitUntilZero {
    info: Rc<RefCell<Info>>,
}

struct Info {
    active: usize,
    blocker: Option<Task>,
}

impl<S: Service> Service for NotifyService<S> {
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn call(&self, message: Self::Request) -> Self::Future {
        self.inner.call(message)
    }
}

impl<S> Drop for NotifyService<S> {
    fn drop(&mut self) {
        let info = match self.info.upgrade() {
            Some(info) => info,
            None => return,
        };
        let mut info = info.borrow_mut();
        info.active -= 1;
        if info.active == 0 {
            if let Some(task) = info.blocker.take() {
                task.unpark();
            }
        }
    }
}

impl Future for WaitUntilZero {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), io::Error> {
        let mut info = self.info.borrow_mut();
        if info.active == 0 {
            Ok(().into())
        } else {
            info.blocker = Some(task::park());
            Ok(Async::NotReady)
        }
    }
}
 
