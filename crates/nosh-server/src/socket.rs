//! A server-side WebSocket that behaves as the `ws` package's does for the event stream: sends
//! complete in order, `close` starts the closing handshake (and gives up after 30 s), and
//! `terminate` drops the connection at once.

use crate::events::EventSocket;
use futures_util::future::LocalBoxFuture;
use futures_util::{SinkExt, StreamExt};
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Error, Message};

type Stream = WebSocketStream<TokioIo<Upgraded>>;
/// How a connection ended: `Err` for a protocol violation (`ws`'s `error` event).
type Outcome = Option<Result<(), String>>;

enum Command {
    Send(String, oneshot::Sender<Result<(), String>>),
    Close(Option<(u16, String)>),
}

pub(crate) struct WebSocketClient {
    commands: mpsc::UnboundedSender<Command>,
    open: Rc<Cell<bool>>,
    buffered: Rc<Cell<usize>>,
    kill: Rc<Notify>,
    ended: watch::Receiver<Outcome>,
}

impl WebSocketClient {
    /// Drives `stream` on a local task until it closes.
    pub(crate) fn start(stream: Stream) -> Rc<Self> {
        let (commands, receiver) = mpsc::unbounded_channel();
        let (outcome, ended) = watch::channel(None);
        let client = Rc::new(Self {
            commands,
            open: Rc::new(Cell::new(true)),
            buffered: Rc::new(Cell::new(0)),
            kill: Rc::new(Notify::new()),
            ended,
        });
        let driver = Driver {
            open: Rc::clone(&client.open),
            buffered: Rc::clone(&client.buffered),
            kill: Rc::clone(&client.kill),
        };
        tokio::task::spawn_local(async move {
            let result = driver.drive(stream, receiver).await;
            driver.open.set(false);
            let _ = outcome.send(Some(result));
        });
        client
    }

    /// `ws.close()` without a status code, as `LocalApiServer.close` closes every client.
    pub(crate) fn close_plain(&self) {
        self.open.set(false);
        let _ = self.commands.send(Command::Close(None));
    }
}

impl EventSocket for WebSocketClient {
    fn send(&self, message: String) -> LocalBoxFuture<'_, Result<(), String>> {
        let (done, result) = oneshot::channel();
        let length = message.len();
        let queued = self.open.get() && self.commands.send(Command::Send(message, done)).is_ok();
        if queued {
            self.buffered.set(self.buffered.get() + length);
        }
        Box::pin(async move {
            if !queued {
                return Err("WebSocket is not open".to_owned());
            }
            result
                .await
                .unwrap_or_else(|_| Err("WebSocket is not open".to_owned()))
        })
    }

    fn close(&self, code: u16, reason: &str) {
        self.open.set(false);
        let _ = self
            .commands
            .send(Command::Close(Some((code, reason.to_owned()))));
    }

    fn terminate(&self) {
        self.kill.notify_one();
    }

    fn buffered_amount(&self) -> usize {
        self.buffered.get()
    }

    fn is_open(&self) -> bool {
        self.open.get()
    }

    fn closed(&self) -> LocalBoxFuture<'_, Result<(), String>> {
        let mut ended = self.ended.clone();
        Box::pin(async move {
            match ended.wait_for(Option::is_some).await {
                Ok(outcome) => outcome.clone().unwrap_or(Ok(())),
                Err(_) => Ok(()),
            }
        })
    }
}

struct Driver {
    open: Rc<Cell<bool>>,
    buffered: Rc<Cell<usize>>,
    kill: Rc<Notify>,
}

impl Driver {
    /// Runs the connection; dropping `stream` on return closes it.
    async fn drive(
        &self,
        mut stream: Stream,
        mut commands: mpsc::UnboundedReceiver<Command>,
    ) -> Result<(), String> {
        let mut closing: Option<tokio::time::Instant> = None;
        loop {
            let give_up = async move {
                match closing {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = self.kill.notified() => return Ok(()),
                () = give_up => return Ok(()),
                command = commands.recv() => match command {
                    None => return Ok(()),
                    Some(Command::Send(message, done)) => {
                        let length = message.len();
                        let sent = tokio::select! {
                            sent = stream.send(Message::text(message)) => sent.map_err(|error| error.to_string()),
                            () = self.kill.notified() => return Ok(()),
                        };
                        self.buffered.set(self.buffered.get().saturating_sub(length));
                        let _ = done.send(sent);
                    }
                    Some(Command::Close(frame)) => {
                        self.open.set(false);
                        if closing.is_some() {
                            continue;
                        }
                        closing = Some(tokio::time::Instant::now() + Duration::from_secs(30));
                        let frame = frame.map(|(code, reason)| CloseFrame {
                            code: CloseCode::from(code),
                            reason: reason.into(),
                        });
                        tokio::select! {
                            closed = stream.close(frame) => if let Err(error) = closed {
                                return ended(error);
                            },
                            () = self.kill.notified() => return Ok(()),
                        }
                    }
                },
                incoming = stream.next() => match incoming {
                    None => return Ok(()),
                    Some(Ok(Message::Close(_))) => self.open.set(false),
                    // Pings are answered by tungstenite; the event stream ignores client messages.
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return ended(error),
                },
            }
        }
    }
}

/// A connection that dropped or finished closing ends quietly; a protocol violation is `ws`'s
/// `error` event.
fn ended(error: Error) -> Result<(), String> {
    match error {
        Error::ConnectionClosed
        | Error::AlreadyClosed
        | Error::Io(_)
        | Error::Protocol(ProtocolError::ResetWithoutClosingHandshake) => Ok(()),
        other => Err(other.to_string()),
    }
}
