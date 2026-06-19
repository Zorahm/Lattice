//! Control-канал клиента к rendezvous-серверу (TCP).
//!
//! Почему TCP, а не поверх датаплейн-UDP: регистрация → матч → синхронный
//! go-сигнал требуют надёжной упорядоченной доставки; реализовывать это поверх
//! UDP — лишняя сложность, TCP даёт даром. Канал отдельный от датаплейна,
//! поэтому STUN/punch/data остаются на одном UDP-сокете, а сигналинг их не
//! трогает (демультиплексировать ничего не нужно).
//!
//! Чтение идёт в фоновом потоке (сервер пушит `Start`/`PeerGone` асинхронно,
//! блокировать на `recv` нельзя), сообщения складываются в канал. Обрыв
//! соединения = `Closed` — вызывающий завершает сессию внятно, не зависая.

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use thiserror::Error;

use lattice_proto::framing::{read_frame, write_frame};
use lattice_proto::{ClientMessage, NatType, RoomId, ServerMessage, PROTOCOL_VERSION};

#[derive(Debug, Error)]
pub enum SignalError {
    #[error("cannot resolve rendezvous address '{0}'")]
    Resolve(String),
    #[error("cannot connect to rendezvous {addr}: {source}")]
    Connect {
        addr: String,
        source: std::io::Error,
    },
    #[error("control channel I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to serialize control message: {0}")]
    Serialize(String),
}

/// Результат ожидания события из control-канала.
#[derive(Debug)]
pub enum SignalRecv {
    /// Пришло сообщение сервера.
    Message(ServerMessage),
    /// За отведённое время сообщений не было (штатно — продолжаем ждать).
    Timeout,
    /// Соединение закрыто/оборвано (reader-поток завершился).
    Closed,
}

/// Подключённый клиент сигналинга. Пишет в сокет напрямую (по одному писателю),
/// читает через фоновый поток → канал.
pub struct SignalingClient {
    write: TcpStream,
    // Mutex — чтобы `SignalingClient` был `Sync` (Receiver сам по себе !Sync):
    // сессия шарит `&SignalingClient` между потоками (control_watch). Реально
    // recv зовёт один поток, contention нулевой.
    events: Mutex<Receiver<ServerMessage>>,
}

impl SignalingClient {
    /// Подключиться к rendezvous-серверу с таймаутом (чтобы недоступный сервер
    /// давал внятную ошибку, а не вис).
    ///
    /// # Errors
    ///
    /// `Resolve` — адрес не резолвится; `Connect` — соединение не установилось.
    pub fn connect(addr: &str, timeout: Duration) -> Result<Self, SignalError> {
        let resolved = addr
            .to_socket_addrs()
            .map_err(|_| SignalError::Resolve(addr.to_string()))?
            .next()
            .ok_or_else(|| SignalError::Resolve(addr.to_string()))?;
        let stream = TcpStream::connect_timeout(&resolved, timeout).map_err(|source| {
            SignalError::Connect {
                addr: addr.to_string(),
                source,
            }
        })?;
        // Nagle off: мелкие control-кадры (go-сигнал) не должны буферизоваться.
        stream.set_nodelay(true)?;

        let read = stream.try_clone()?;
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("signal-reader".into())
            .spawn(move || reader_loop(read, &tx))?;

        Ok(Self {
            write: stream,
            events: Mutex::new(rx),
        })
    }

    /// Зарегистрироваться в комнате: отправить `Register` со своим srflx и NAT.
    ///
    /// # Errors
    ///
    /// `Serialize`/`Io` — не удалось сериализовать или отправить сообщение.
    pub fn register(&mut self, room: RoomId, srflx: &str, nat: NatType) -> Result<(), SignalError> {
        self.send(&ClientMessage::Register {
            protocol_version: PROTOCOL_VERSION,
            room,
            srflx: srflx.to_string(),
            nat,
        })
    }

    /// Отправить произвольное клиентское сообщение (`PunchFailed`/`PunchOk`/`Bye`).
    ///
    /// # Errors
    ///
    /// `Serialize` — сбой serde; `Io` — запись в сокет провалилась.
    pub fn send(&mut self, msg: &ClientMessage) -> Result<(), SignalError> {
        let json = serde_json::to_vec(msg).map_err(|e| SignalError::Serialize(e.to_string()))?;
        write_frame(&mut self.write, &json)?;
        Ok(())
    }

    /// Дождаться события сервера до `timeout`. `Timeout`/`Closed` — не ошибки, а
    /// явные состояния для машины установления соединения.
    #[must_use]
    pub fn recv(&self, timeout: Duration) -> SignalRecv {
        // Отравленный mutex (паника другого потока) трактуем как закрытие канала.
        let Ok(rx) = self.events.lock() else {
            return SignalRecv::Closed;
        };
        match rx.recv_timeout(timeout) {
            Ok(msg) => SignalRecv::Message(msg),
            Err(RecvTimeoutError::Timeout) => SignalRecv::Timeout,
            Err(RecvTimeoutError::Disconnected) => SignalRecv::Closed,
        }
    }
}

/// Фоновый цикл чтения: парсит кадры в `ServerMessage` и шлёт в канал. Любой
/// EOF/ошибка/битый кадр завершает поток → канал закрывается → вызывающий
/// получит `Closed`.
fn reader_loop(mut stream: TcpStream, tx: &mpsc::Sender<ServerMessage>) {
    loop {
        match read_frame(&mut stream) {
            Ok(Some(bytes)) => match serde_json::from_slice::<ServerMessage>(&bytes) {
                Ok(msg) => {
                    if tx.send(msg).is_err() {
                        return; // приёмник ушёл — клиент закрылся.
                    }
                }
                Err(e) => log::warn!("signaling: malformed server message dropped: {e}"),
            },
            Ok(None) => {
                log::info!("signaling: server closed control channel");
                return;
            }
            Err(e) => {
                log::warn!("signaling: control channel read error: {e}");
                return;
            }
        }
    }
}
