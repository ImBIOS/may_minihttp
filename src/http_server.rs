//! http server implementation on top of `MAY`

use std::error::Error;
use std::io::{self, Read, Write};
use std::net::ToSocketAddrs;
use std::sync::Arc;

use sequencer::Seq;
use may::coroutine;
use may::net::TcpListener;
use request::{self, Request};
use bytes::{BufMut, BytesMut};
use response::{self, Response};

/// the http service trait
/// user code should supply a type that impl the `call` method for the http server
///
pub trait HttpService {
    fn call(&self, _request: Request) -> io::Result<Response>;
}

macro_rules! t {
    ($e: expr) => (match $e {
        Ok(val) => val,
        Err(ref err) if err.kind() == io::ErrorKind::ConnectionReset ||
                        err.kind() == io::ErrorKind::UnexpectedEof=> {
            // info!("http server read req: connection closed");
            return;
        }
        Err(err) => {
            error!("call = {:?}\nerr = {:?}", stringify!($e), err);
            return;
        }
    })
}

macro_rules! t_c {
    ($e: expr) => (match $e {
        Ok(val) => val,
        Err(err) => {
            error!("call = {:?}\nerr = {:?}", stringify!($e), err);
            continue;
        }
    })
}

fn internal_error_rsp(e: io::Error) -> Response {
    error!("error in service: err = {:?}", e);
    let mut err_rsp = Response::new();
    err_rsp.status_code(500, "Internal Server Error");
    err_rsp.body(e.description());
    err_rsp
}

/// this is the generic type http server
/// with a type parameter that impl `HttpService` trait
///
pub struct HttpServer<T>(pub T);

impl<T: HttpService + Send + Sync + 'static> HttpServer<T> {
    /// Spawns the http service, binding to the given address
    /// return a coroutine that you can cancel it when need to stop the service
    pub fn start<L: ToSocketAddrs>(self, addr: L) -> io::Result<coroutine::JoinHandle<()>> {
        let listener = TcpListener::bind(addr)?;
        go!(
            coroutine::Builder::new().name("TcpServer".to_owned()),
            move || {
                let server = Arc::new(self);
                for stream in listener.incoming() {
                    let mut stream = t_c!(stream);
                    let server = server.clone();
                    go!(move || {
                        let mut buf = BytesMut::with_capacity(512);
                        let mut rsp = BytesMut::with_capacity(512);
                        loop {
                            match t!(request::decode(&mut buf)) {
                                None => {
                                    // need more data
                                    if buf.remaining_mut() < 256 {
                                        buf.reserve(512);
                                    }
                                    let n = {
                                        let read_buf = unsafe { buf.bytes_mut() };
                                        t!(stream.read(read_buf))
                                    };
                                    if n == 0 {
                                        //connection was closed
                                        return;
                                    }
                                    unsafe { buf.advance_mut(n) };
                                }
                                Some(req) => {
                                    let ret = server.0.call(req).unwrap_or_else(internal_error_rsp);
                                    response::encode(ret, &mut rsp);

                                    // send the result back to client
                                    t!(stream.write_all(rsp.as_ref()));
                                    rsp.clear();
                                }
                            }
                        }
                    });
                }
            }
        )
    }
}

/// this is the pipeline type http server
/// with a type parameter that impl `HttpService` trait
///
pub struct HttpPipelineServer<T>(pub T);

impl<T: HttpService + Send + Sync + 'static> HttpPipelineServer<T> {
    /// Spawns the http service, binding to the given address
    /// return a coroutine that you can cancel it when need to stop the service
    pub fn start<L: ToSocketAddrs>(self, addr: L) -> io::Result<coroutine::JoinHandle<()>> {
        let listener = TcpListener::bind(addr)?;
        go!(
            coroutine::Builder::new().name("TcpServer".to_owned()),
            move || {
                let server = Arc::new(self);
                for stream in listener.incoming() {
                    let stream = t_c!(stream);
                    let mut reader = t_c!(stream.try_clone());
                    let writer = Seq::new(stream);
                    let server = server.clone();
                    go!(move || {
                        let mut buf = BytesMut::with_capacity(4096);
                        loop {
                            match t!(request::decode(&mut buf)) {
                                None => {
                                    // need more data
                                    if buf.remaining_mut() < 1024 {
                                        buf.reserve(4096);
                                    }
                                    let n = {
                                        let read_buf = unsafe { buf.bytes_mut() };
                                        t!(reader.read(read_buf))
                                    };
                                    if n == 0 {
                                        //connection was closed
                                        return;
                                    }
                                    unsafe { buf.advance_mut(n) };
                                }
                                Some(req) => {
                                    let writer = writer.next();
                                    let server = server.clone();
                                    // async process the request
                                    go!(move || {
                                        let ret =
                                            server.0.call(req).unwrap_or_else(internal_error_rsp);
                                        let mut rsp = BytesMut::with_capacity(512);
                                        response::encode(ret, &mut rsp);
                                        let mut writer = writer.lock();
                                        // send the result back to client
                                        t!(writer.write_all(rsp.as_ref()));
                                    });
                                }
                            }
                        }
                    });
                }
            }
        )
    }
}
