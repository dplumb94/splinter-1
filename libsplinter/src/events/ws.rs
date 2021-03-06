// Copyright 2019 Cargill Incorporated
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! WebSocket Module.
//!
//! Module for establishing WebSocket connections with Splinter Services.
//!
//!```
//! use std::{thread::sleep, time};
//! use splinter::events::{WsResponse, WebSocketClient, Reactor, ParseBytes};
//!
//! let reactor = Reactor::new();
//!
//! let mut ws = WebSocketClient::new(
//!    "http://echo.websocket.org", |ctx, msg: Vec<u8>| {
//!    if let Ok(s) = String::from_utf8(msg.clone()) {
//!         println!("Recieved {}", s);
//!    } else {
//!       println!("malformed message: {:?}", msg);
//!    };
//!    WsResponse::Text("welcome to earth!!!".to_string())
//! });
//!
//! // Optional callback for when connection is established
//! ws.on_open(|_| {
//!    println!("sending message");
//!    WsResponse::Text("hello, world".to_string())
//! });
//!
//! ws.on_error(move |err, ctx| {
//!     println!("Error!: {:?}", err);
//!     // ws instance can be used to restart websocket
//!     ctx.start_ws().unwrap();
//!     Ok(())
//! });
//!
//! reactor.igniter().start_ws(&ws).unwrap();
//!
//! sleep(time::Duration::from_secs(1));
//! println!("stopping");
//! reactor.shutdown().unwrap();
//! ```

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use actix_http::ws;
use awc::ws::{CloseCode, CloseReason, Codec, Frame, Message};
use crossbeam_channel::{bounded, Receiver};
use futures::{
    future::{self, Either},
    sink::Wait,
    Future,
};
use hyper::{self, header, upgrade::Upgraded, Body, Client, Request, StatusCode};
use tokio::codec::{Decoder, Framed};
use tokio::prelude::*;

use crate::events::{Igniter, ParseError, WebSocketError};

type OnErrorHandle<T> =
    dyn Fn(&WebSocketError, Context<T>) -> Result<(), WebSocketError> + Send + Sync + 'static;

const MAX_FRAME_SIZE: usize = 10_000_000;

/// Wrapper around future created by `WebSocketClient`. In order for
/// the future to run it must be passed to `Igniter::start_ws`
pub struct Listen {
    future: Box<dyn Future<Item = (), Error = WebSocketError> + Send + 'static>,
    running: Arc<AtomicBool>,
    receiver: Receiver<Result<(), WebSocketError>>,
}

impl Listen {
    pub fn into_shutdown_handle(
        self,
    ) -> (
        Box<dyn Future<Item = (), Error = WebSocketError> + Send + 'static>,
        ShutdownHandle,
    ) {
        (
            self.future,
            ShutdownHandle {
                running: self.running,
                receiver: self.receiver,
            },
        )
    }
}

#[derive(Clone)]
pub struct ShutdownHandle {
    running: Arc<AtomicBool>,
    receiver: Receiver<Result<(), WebSocketError>>,
}

impl ShutdownHandle {
    /// Sends shutdown message to websocket
    pub fn shutdown(self) -> Result<(), WebSocketError> {
        self.running.store(false, Ordering::SeqCst);
        self.receiver.recv()?
    }
}

/// WebSocket client. Configures Websocket connection and produces `Listen` future.
pub struct WebSocketClient<T: ParseBytes<T> + 'static = Vec<u8>> {
    url: String,
    on_message: Arc<dyn Fn(Context<T>, T) -> WsResponse + Send + Sync + 'static>,
    on_open: Option<Arc<dyn Fn(Context<T>) -> WsResponse + Send + Sync + 'static>>,
    on_error: Option<Arc<OnErrorHandle<T>>>,
}

impl<T: ParseBytes<T> + 'static> Clone for WebSocketClient<T> {
    fn clone(&self) -> Self {
        WebSocketClient {
            url: self.url.clone(),
            on_message: self.on_message.clone(),
            on_open: self.on_open.clone(),
            on_error: self.on_error.clone(),
        }
    }
}

impl<T: ParseBytes<T> + 'static> WebSocketClient<T> {
    pub fn new<F>(url: &str, on_message: F) -> Self
    where
        F: Fn(Context<T>, T) -> WsResponse + Send + Sync + 'static,
    {
        Self {
            url: url.to_string(),
            on_message: Arc::new(on_message),
            on_open: None,
            on_error: None,
        }
    }

    pub fn url(&self) -> String {
        self.url.clone()
    }

    /// Adds optional `on_open` closure. This closer is called after a connection is initially
    /// established with the server, and is used for printing debug information and sending initial
    /// messages to server if necessary.
    pub fn on_open<F>(&mut self, on_open: F)
    where
        F: Fn(Context<T>) -> WsResponse + Send + Sync + 'static,
    {
        self.on_open = Some(Arc::new(on_open));
    }

    /// Adds optional `on_error` closure. This closure would be called when the Websocket has closed due to
    /// an unexpected error. This callback should be used to shutdown any IO resources being used by the
    /// Websocket or to reestablish the connection if appropriate.
    pub fn on_error<F>(&mut self, on_error: F)
    where
        F: Fn(&WebSocketError, Context<T>) -> Result<(), WebSocketError> + Send + Sync + 'static,
    {
        self.on_error = Some(Arc::new(on_error));
    }

    /// Returns `Listen` for WebSocket.
    pub fn listen(&self, igniter: Igniter) -> Result<Listen, WebSocketError> {
        let url = self.url.clone();
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();
        let on_open = self
            .on_open
            .clone()
            .unwrap_or_else(|| Arc::new(|_| WsResponse::Empty));
        let on_message = self.on_message.clone();
        let on_error = self
            .on_error
            .clone()
            .unwrap_or_else(|| Arc::new(|_, _| Ok(())));

        let context = Context::new(igniter, self.clone());

        let (sender, receiver) = bounded(1);

        debug!("starting: {}", url);

        let request = Request::builder()
            .uri(url)
            .header(header::UPGRADE, "websocket")
            .header(header::CONNECTION, "Upgrade")
            .header(header::SEC_WEBSOCKET_VERSION, "13")
            .header(header::SEC_WEBSOCKET_KEY, "13")
            .body(Body::empty())
            .map_err(|err| WebSocketError::RequestBuilderError(format!("{:?}", err)))?;

        let future = Box::new(
            Client::new()
                .request(request)
                .and_then(|res| {
                    if res.status() != StatusCode::SWITCHING_PROTOCOLS {
                        error!("The server didn't upgrade: {}", res.status());
                    }
                    debug!("response: {:?}", res);

                    res.into_body().on_upgrade()
                })
                .map_err(|err| {
                    error!("Client Error: {:?}", err);
                    WebSocketError::from(err)
                })
                .and_then(move |upgraded| {
                    let codec = Codec::new().max_size(MAX_FRAME_SIZE).client_mode();
                    let framed = codec.framed(upgraded);
                    let (sink, stream) = framed.split();

                    let mut blocking_sink = sink.wait();

                    if let Err(err) = handle_response(&mut blocking_sink, on_open(context.clone()))
                    {
                        if let Err(err) = sender.send(Err(err)) {
                            error!("Failed to send response to shutdown handle: {}", err);
                        }
                        return Either::A(future::ok(()));
                    }

                    Either::B(
                        stream
                            .map_err(|err| {
                                error!("Protocol Error: {:?}", err);
                                WebSocketError::from(err)
                            })
                            .take_while(move |message| {
                                let status = match message {
                                    Frame::Text(msg) | Frame::Binary(msg) => {
                                        let bytes = if let Some(bytes) = msg {
                                            bytes.to_vec()
                                        } else {
                                            Vec::new()
                                        };
                                        let result = T::from_bytes(&bytes)
                                            .map_err(|parse_error| {
                                                error!(
                                                    "Failed to parse server message {}",
                                                    parse_error
                                                );
                                                if let Err(protocol_error) = do_shutdown(
                                                    &mut blocking_sink,
                                                    CloseCode::Protocol,
                                                ) {
                                                    WebSocketError::ParserError {
                                                        parse_error,
                                                        shutdown_error: Some(protocol_error),
                                                    }
                                                } else {
                                                    WebSocketError::ParserError {
                                                        parse_error,
                                                        shutdown_error: None,
                                                    }
                                                }
                                            })
                                            .and_then(|message| {
                                                handle_response(
                                                    &mut blocking_sink,
                                                    on_message(context.clone(), message),
                                                )
                                            });

                                        if let Err(err) = result {
                                            ConnectionStatus::UnexpectedClose(err)
                                        } else {
                                            ConnectionStatus::Open
                                        }
                                    }
                                    Frame::Ping(msg) => {
                                        debug!("Received Ping {} sending pong", msg);
                                        if let Err(err) = handle_response(
                                            &mut blocking_sink,
                                            WsResponse::Pong(msg.to_string()),
                                        ) {
                                            ConnectionStatus::UnexpectedClose(err)
                                        } else {
                                            ConnectionStatus::Open
                                        }
                                    }
                                    Frame::Pong(msg) => {
                                        debug!("Received Pong {}", msg);
                                        ConnectionStatus::Open
                                    }
                                    Frame::Close(msg) => {
                                        debug!("Received close message {:?}", msg);
                                        let result =
                                            do_shutdown(&mut blocking_sink, CloseCode::Normal)
                                                .map_err(WebSocketError::from);
                                        ConnectionStatus::Close(result)
                                    }
                                };

                                match (running_clone.load(Ordering::SeqCst), status) {
                                    (true, ConnectionStatus::Open) => future::ok(true),
                                    (false, ConnectionStatus::Open) => {
                                        let shutdown_result =
                                            do_shutdown(&mut blocking_sink, CloseCode::Normal)
                                                .map_err(WebSocketError::from);
                                        if let Err(err) = sender.send(shutdown_result) {
                                            error!(
                                                "Failed to send response to shutdown handle: {}",
                                                err
                                            );
                                        }
                                        future::ok(false)
                                    }
                                    (_, ConnectionStatus::UnexpectedClose(original_error)) => {
                                        let result = on_error(&original_error, context.clone())
                                            .map_err(|on_fail_error| WebSocketError::OnFailError {
                                                original_error: Box::new(original_error),
                                                on_fail_error: Box::new(on_fail_error),
                                            });
                                        if let Err(err) = sender.send(result) {
                                            error!(
                                                "Failed to send response to shutdown handle: {}",
                                                err
                                            );
                                        }
                                        future::ok(false)
                                    }
                                    (_, ConnectionStatus::Close(res)) => {
                                        if let Err(err) = sender.send(res) {
                                            error!(
                                                "Failed to send response to shutdown handle: {}",
                                                err
                                            );
                                        }
                                        future::ok(false)
                                    }
                                }
                            })
                            .for_each(|_| future::ok(())),
                    )
                }),
        );

        Ok(Listen {
            future,
            running,
            receiver,
        })
    }
}

fn handle_response(
    wait_sink: &mut Wait<stream::SplitSink<Framed<Upgraded, Codec>>>,
    res: WsResponse,
) -> Result<(), WebSocketError> {
    match res {
        WsResponse::Text(msg) => wait_sink
            .send(Message::Text(msg))
            .and_then(|_| wait_sink.flush())
            .or_else(|protocol_error| {
                error!("Error occurred while handling message {:?}", protocol_error);
                if let Err(shutdown_error) = do_shutdown(wait_sink, CloseCode::Protocol) {
                    Err(WebSocketError::AbnormalShutdownError {
                        protocol_error,
                        shutdown_error,
                    })
                } else {
                    Err(WebSocketError::from(protocol_error))
                }
            }),
        WsResponse::Bytes(bytes) => wait_sink
            .send(Message::Binary(bytes.as_slice().into()))
            .and_then(|_| wait_sink.flush())
            .or_else(|protocol_error| {
                error!("Error occurred while handling message {:?}", protocol_error);
                if let Err(shutdown_error) = do_shutdown(wait_sink, CloseCode::Protocol) {
                    Err(WebSocketError::AbnormalShutdownError {
                        protocol_error,
                        shutdown_error,
                    })
                } else {
                    Err(WebSocketError::from(protocol_error))
                }
            }),
        WsResponse::Pong(msg) => wait_sink
            .send(Message::Pong(msg))
            .or_else(|protocol_error| {
                error!("Error occurred while handling message {:?}", protocol_error);
                if let Err(shutdown_error) = do_shutdown(wait_sink, CloseCode::Protocol) {
                    Err(WebSocketError::AbnormalShutdownError {
                        protocol_error,
                        shutdown_error,
                    })
                } else {
                    Err(WebSocketError::from(protocol_error))
                }
            }),
        WsResponse::Close => {
            do_shutdown(wait_sink, CloseCode::Normal).map_err(WebSocketError::from)
        }
        WsResponse::Empty => Ok(()),
    }
}

fn do_shutdown(
    blocking_sink: &mut Wait<stream::SplitSink<Framed<Upgraded, Codec>>>,
    close_code: CloseCode,
) -> Result<(), ws::ProtocolError> {
    blocking_sink
        .send(Message::Close(Some(CloseReason::from(close_code))))
        .and_then(|_| blocking_sink.flush())
        .and_then(|_| {
            debug!("Socket connection closed successfully");
            blocking_sink.close()
        })
        .or_else(|_| blocking_sink.close())
}

/// Websocket context object. It contains an Igniter pointing
/// to the Reactor on which the websocket future is running and
/// a copy of the WebSocketClient object.
#[derive(Clone)]
pub struct Context<T: ParseBytes<T> + 'static> {
    igniter: Igniter,
    ws: WebSocketClient<T>,
}

impl<T: ParseBytes<T> + 'static> Context<T> {
    pub fn new(igniter: Igniter, ws: WebSocketClient<T>) -> Self {
        Self { igniter, ws }
    }

    /// Starts an instance of the Context's websocket.
    pub fn start_ws(&self) -> Result<(), WebSocketError> {
        self.igniter.start_ws(&self.ws)
    }

    /// Returns a copy of the igniter used to start the websocket.
    pub fn igniter(&self) -> Igniter {
        self.igniter.clone()
    }
}

enum ConnectionStatus {
    Open,
    UnexpectedClose(WebSocketError),
    Close(Result<(), WebSocketError>),
}

/// Response object returned by `WebSocket` client callbacks.
#[derive(Debug)]
pub enum WsResponse {
    Empty,
    Close,
    Pong(String),
    Text(String),
    Bytes(Vec<u8>),
}

pub trait ParseBytes<T: 'static>: Send + Sync + Clone {
    fn from_bytes(bytes: &[u8]) -> Result<T, ParseError>;
}

impl ParseBytes<Vec<u8>> for Vec<u8> {
    fn from_bytes(bytes: &[u8]) -> Result<Vec<u8>, ParseError> {
        Ok(bytes.to_vec())
    }
}
