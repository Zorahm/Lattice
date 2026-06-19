//! QUIC-реализация `trait Transport` (Фаза 4) — маскировка под HTTP/3.
//!
//! ## Двойное шифрование — осознанно
//!
//! Внутрь QUIC едут УЖЕ зашифрованные датаграммы `[nonce||ChaCha20-Poly1305]`
//! (E2E shared-key из Фаз 1-3). QUIC добавляет ВНЕШНИЙ TLS-слой РАДИ МАСКИРОВКИ,
//! не ради защиты — внутренний слой и так защищает. Сервер-relay по-прежнему
//! видит только ciphertext внутреннего слоя (он вообще не в QUIC-пути: QUIC —
//! это прямой p2p после punch; см. ниже).
//!
//! ## QUIC DATAGRAM, не стримы
//!
//! Датаплейн едет в QUIC DATAGRAM-фреймах (RFC 9221), НЕ в надёжных стримах:
//! VPN-трафик и так lossy-tolerant (как UDP), а ретрансмиты QUIC-стрима поверх
//! TCP-игры/UDP-видео ломали бы latency (head-of-line blocking). DATAGRAM даёт
//! UDP-семантику внутри QUIC-обёртки.
//!
//! ## Sync↔async мост
//!
//! Остальной код синхронный (std-потоки, без tokio — контракт сервера). quinn
//! асинхронный. Мост: QUIC-соединение живёт в фоновом tokio-рантайме (свой поток),
//! `send`/`recv` общаются с ним через каналы. `recv` — `recv_timeout`, чтобы
//! поток приёма мог опрашивать shutdown (как у `UdpTransport`).
//!
//! ## Роли (асимметрия QUIC)
//!
//! QUIC connection-oriented: одна сторона слушает (`QuicListener`), другая
//! дозванивается (`QuicTransport::connect`). В mesh/dynamic роль назначается
//! через signaling (детерминированно, напр. меньший peer-id слушает), чтобы не
//! было молчаливого рассинхрона «обе слушают / обе звонят». Согласование самого
//! факта QUIC (а не UDP) — тоже через signaling.
//!
//! ## MTU
//!
//! QUIC поверх UDP отъедает заголовки сверх и так тесного 1380. Эффективный
//! предел DATAGRAM — `max_datagram_size()`; фрейм больше него отправить нельзя
//! (`send` вернёт ошибку, фрейм дропается с логом). TAP MTU при QUIC надо
//! опускать на `QUIC_DATAGRAM_OVERHEAD` (см. `transport::quic_effective_mtu`).

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::{self as std_mpsc, Receiver as StdReceiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use quinn::{Connection, Endpoint, EndpointConfig, TokioRuntime};
use thiserror::Error;
use tokio::runtime::Runtime;
use tokio::sync::mpsc as tokio_mpsc;

use crate::transport::quic_tls::{self, TlsError};
use crate::transport::{Transport, TransportError};

/// Консервативная оценка оверхеда QUIC+DATAGRAM-заголовков поверх UDP-payload.
/// QUIC short-header (~1) + connection-id (0-20, у нас 0) + packet number (1-4) +
/// DATAGRAM frame type/len (~3) + AEAD-тег QUIC (16). Берём с запасом 64 —
/// лучше чуть занизить MTU, чем словить тихий дроп негабаритной датаграммы.
pub const QUIC_DATAGRAM_OVERHEAD: usize = 64;

/// Ошибки QUIC-транспорта. Сетевые несут контекст; handshake-провал отделён,
/// чтобы selector мог по нему эскалировать/откатиться (порт/SNI могли зарезать).
#[derive(Debug, Error)]
pub enum QuicError {
    #[error("tls/alpn config error: {0}")]
    Tls(#[from] TlsError),
    #[error("quic endpoint bind on {addr} failed: {source}")]
    Bind { addr: SocketAddr, source: io::Error },
    #[error("quic handshake to {addr} failed (port/SNI may be filtered): {reason}")]
    Handshake { addr: SocketAddr, reason: String },
    #[error("quic connection accept failed: {0}")]
    Accept(String),
    #[error("tokio runtime build failed: {0}")]
    Runtime(io::Error),
}

/// Слушающая сторона QUIC. Биндит endpoint сразу (даёт `local_addr` до приёма),
/// `accept` затем блокирует до одного соединения. Разделение нужно, чтобы
/// дозванивающийся пир узнал адрес до того, как мы заблокируемся на `accept`.
pub struct QuicListener {
    rt: Runtime,
    endpoint: Endpoint,
    local: SocketAddr,
}

impl QuicListener {
    /// Забиндить QUIC-сервер на новом сокете (self-signed cert на `sni`, ALPN h3).
    ///
    /// # Errors
    ///
    /// `QuicError::Tls`/`Bind` — сбой конфигурации или бинда сокета.
    pub fn bind(addr: SocketAddr, sni: &str) -> Result<Self, QuicError> {
        let socket = UdpSocket::bind(addr).map_err(|source| QuicError::Bind { addr, source })?;
        Self::from_socket(socket, sni)
    }

    /// Слушать QUIC поверх УЖЕ существующего UDP-сокета — ключевое для Фазы 4:
    /// после hole-punching NAT-маппинг привязан к конкретному (ip:port), и QUIC
    /// обязан идти через ТОТ ЖЕ сокет, иначе пир не достучится (новый порт = новый
    /// маппинг). Caller отдаёт punched-сокет (`try_clone`) и прекращает слать по
    /// нему сырой UDP — теперь им владеет quinn.
    ///
    /// # Errors
    ///
    /// `QuicError::Tls` — конфиг; `QuicError::Bind` — quinn не принял сокет.
    pub fn from_socket(socket: UdpSocket, sni: &str) -> Result<Self, QuicError> {
        let server_config = quic_tls::server_config(sni)?;
        let rt = build_runtime()?;
        let addr = socket.local_addr().map_err(|source| QuicError::Bind {
            addr: unspecified(),
            source,
        })?;
        let endpoint = rt
            .block_on(async {
                Endpoint::new(
                    EndpointConfig::default(),
                    Some(server_config),
                    socket,
                    Arc::new(TokioRuntime),
                )
            })
            .map_err(|source| QuicError::Bind { addr, source })?;
        Ok(Self { rt, endpoint, local: addr })
    }

    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Принять ОДНО входящее соединение и отдать готовый транспорт. Блокирует до
    /// соединения. `recv_timeout` — пауза опроса shutdown в `recv`.
    ///
    /// # Errors
    ///
    /// `QuicError::Accept` — пир не пришёл / handshake провалился.
    pub fn accept(self, recv_timeout: Duration) -> Result<QuicTransport, QuicError> {
        let QuicListener { rt, endpoint, local } = self;
        let conn = rt.block_on(async {
            let incoming = endpoint
                .accept()
                .await
                .ok_or_else(|| QuicError::Accept("endpoint closed before any connection".into()))?;
            incoming
                .await
                .map_err(|e| QuicError::Accept(e.to_string()))
        })?;
        Ok(spawn_driver(rt, endpoint, conn, local, recv_timeout))
    }
}

impl QuicTransport {
    /// Дозвониться до слушающего пира (ALPN h3, SNI `sni`, любой серверный cert).
    /// `bind` — локальный UDP-адрес (обычно тот же, через который шёл punch).
    /// `handshake_timeout` явно ограничивает рукопожатие: зарезанный порт/SNI
    /// должен падать БЫСТРО, чтобы selector сразу откатился, а не висел до
    /// idle-таймаута QUIC (иначе auto-эскалация томит пользователя ~30с).
    ///
    /// # Errors
    ///
    /// `QuicError::Bind` — локальный бинд; `Handshake` — рукопожатие не прошло
    /// или не уложилось в `handshake_timeout` (порт/SNI могли быть зарезаны —
    /// selector по этому эскалирует/откатывается).
    pub fn connect(
        bind: SocketAddr,
        peer: SocketAddr,
        sni: &str,
        handshake_timeout: Duration,
        recv_timeout: Duration,
    ) -> Result<Self, QuicError> {
        let socket = UdpSocket::bind(bind).map_err(|source| QuicError::Bind { addr: bind, source })?;
        Self::connect_with_socket(socket, peer, sni, handshake_timeout, recv_timeout)
    }

    /// Дозвониться поверх УЖЕ существующего UDP-сокета (см. `QuicListener::
    /// from_socket` — переиспользование punched NAT-маппинга). Дозванивающаяся
    /// сторона определяется детерминированно через signaling (см. `selector::
    /// quic_listener_first`), чтобы не было «обе слушают / обе звонят».
    ///
    /// # Errors
    ///
    /// `QuicError::Bind` — quinn не принял сокет; `Handshake` — рукопожатие не
    /// прошло/не уложилось в `handshake_timeout` (порт/SNI могли быть зарезаны).
    pub fn connect_with_socket(
        socket: UdpSocket,
        peer: SocketAddr,
        sni: &str,
        handshake_timeout: Duration,
        recv_timeout: Duration,
    ) -> Result<Self, QuicError> {
        let client_config = quic_tls::client_config()?;
        let rt = build_runtime()?;
        let local = socket.local_addr().map_err(|source| QuicError::Bind {
            addr: unspecified(),
            source,
        })?;
        let (endpoint, conn) = rt.block_on(async {
            let mut ep = Endpoint::new(EndpointConfig::default(), None, socket, Arc::new(TokioRuntime))
                .map_err(|source| QuicError::Bind { addr: local, source })?;
            ep.set_default_client_config(client_config);
            let connecting = ep
                .connect(peer, sni)
                .map_err(|e| QuicError::Handshake { addr: peer, reason: e.to_string() })?;
            let conn = match tokio::time::timeout(handshake_timeout, connecting).await {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    return Err(QuicError::Handshake { addr: peer, reason: e.to_string() })
                }
                Err(_) => {
                    return Err(QuicError::Handshake {
                        addr: peer,
                        reason: format!("handshake timed out after {handshake_timeout:?}"),
                    })
                }
            };
            Ok::<_, QuicError>((ep, conn))
        })?;
        Ok(spawn_driver(rt, endpoint, conn, local, recv_timeout))
    }
}

/// `0.0.0.0:0` — заглушка адреса для контекста ошибки, когда реальный неизвестен.
fn unspecified() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 0))
}

/// QUIC-транспорт: одно соединение, датаплейн в DATAGRAM. `send` игнорирует
/// `addr` (как `RelayTransport`) — соединение одно, адрес зашит в нём.
pub struct QuicTransport {
    // Рантайм держим живым: в нём крутится драйвер соединения. Дроп → закрытие.
    _rt: Runtime,
    out_tx: tokio_mpsc::UnboundedSender<Bytes>,
    // `Mutex` вокруг std-`Receiver`: он `!Sync`, а датаплейн-сессия делит
    // `&Transport` между потоками (tap→net шлёт, net→tap принимает). recv —
    // единственный лочащий, contention нулевой; так `QuicTransport: Sync`.
    in_rx: Mutex<StdReceiver<Vec<u8>>>,
    peer: SocketAddr,
    local: SocketAddr,
    recv_timeout: Duration,
    max_datagram: usize,
}

impl QuicTransport {
    /// Максимальный размер DATAGRAM, который примет соединение. Для пересчёта
    /// эффективного MTU датаплейна (TAP MTU надо держать ниже).
    #[must_use]
    pub fn max_datagram_size(&self) -> usize {
        self.max_datagram
    }

    #[must_use]
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }
}

/// Собрать фоновый рантайм + драйвер соединения и вернуть синхронный транспорт.
/// Драйвер пампит датаграммы между каналами и quinn-соединением.
fn spawn_driver(
    rt: Runtime,
    endpoint: Endpoint,
    conn: Connection,
    local: SocketAddr,
    recv_timeout: Duration,
) -> QuicTransport {
    let peer = conn.remote_address();
    // max_datagram_size: None ⇒ пир не поддерживает DATAGRAM. На практике quinn
    // включает их по умолчанию; fallback к консервативному 1200, чтобы send не
    // паниковал, а крупное дропалось с логом.
    let max_datagram = conn.max_datagram_size().unwrap_or(1200);

    let (out_tx, out_rx) = tokio_mpsc::unbounded_channel::<Bytes>();
    let (in_tx, in_rx) = std_mpsc::channel::<Vec<u8>>();

    // Драйвер владеет endpoint+conn (живут, пока жив рантайм/транспорт).
    rt.spawn(driver(endpoint, conn, out_rx, in_tx));

    QuicTransport {
        _rt: rt,
        out_tx,
        in_rx: Mutex::new(in_rx),
        peer,
        local,
        recv_timeout,
        max_datagram,
    }
}

/// Цикл драйвера: исходящие из канала → `send_datagram`; входящие
/// `read_datagram` → в std-канал приёма. Завершается при закрытии любого конца.
async fn driver(
    endpoint: Endpoint,
    conn: Connection,
    mut out_rx: tokio_mpsc::UnboundedReceiver<Bytes>,
    in_tx: std_mpsc::Sender<Vec<u8>>,
) {
    loop {
        tokio::select! {
            outbound = out_rx.recv() => match outbound {
                Some(bytes) => {
                    if let Err(e) = conn.send_datagram(bytes) {
                        // Негабаритный фрейм / соединение закрылось — лог, не паника.
                        log::warn!("quic: send_datagram failed: {e}");
                    }
                }
                None => break, // транспорт сдропан (send-half закрыт).
            },
            inbound = conn.read_datagram() => match inbound {
                Ok(bytes) => {
                    if in_tx.send(bytes.to_vec()).is_err() {
                        break; // приёмная сторона ушла.
                    }
                }
                Err(e) => {
                    log::info!("quic: connection closed: {e}");
                    break;
                }
            },
        }
    }
    // Корректно гасим endpoint, чтобы FIN/CLOSE ушли пиру (а не таймаут).
    endpoint.close(0u32.into(), b"bye");
}

impl Transport for QuicTransport {
    fn send(&self, _addr: SocketAddr, data: &[u8]) -> Result<(), TransportError> {
        if data.len() > self.max_datagram {
            // Тихую фрагментацию/дроп не глотаем — явная ошибка наружу (caller
            // логирует). MTU должен быть выставлен так, чтобы сюда не попадать.
            return Err(TransportError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("frame {} exceeds quic datagram limit {}", data.len(), self.max_datagram),
            )));
        }
        self.out_tx
            .send(Bytes::copy_from_slice(data))
            .map_err(|_| TransportError::Io(io::Error::new(io::ErrorKind::BrokenPipe, "quic driver gone")))
    }

    fn recv(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr), TransportError> {
        let rx = self
            .in_rx
            .lock()
            .map_err(|_| TransportError::Io(io::Error::other("quic recv mutex poisoned")))?;
        match rx.recv_timeout(self.recv_timeout) {
            Ok(datagram) => {
                let n = datagram.len().min(buf.len());
                buf[..n].copy_from_slice(&datagram[..n]);
                Ok((n, self.peer))
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => Err(TransportError::WouldBlock),
            Err(std_mpsc::RecvTimeoutError::Disconnected) => Err(TransportError::Io(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "quic driver gone",
            ))),
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }
}

/// Однопоточный (1 worker) multi-thread рантайм: драйвер крутится в фоне, пока
/// синхронные `send`/`recv` идут из потоков датаплейна. `enable_all` — io+time
/// для quinn-udp и таймеров QUIC.
fn build_runtime() -> Result<Runtime, QuicError> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(QuicError::Runtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn loopback() -> SocketAddr {
        "127.0.0.1:0".parse().expect("addr")
    }

    #[test]
    fn quic_loopback_datagram_roundtrip() {
        let timeout = Duration::from_millis(200);
        let listener = QuicListener::bind(loopback(), "example.com").expect("bind");
        let server_addr = listener.local_addr();

        let server_thread = thread::spawn(move || listener.accept(Duration::from_millis(200)));

        let client = QuicTransport::connect(
            loopback(),
            server_addr,
            "example.com",
            Duration::from_secs(5),
            timeout,
        )
        .expect("connect");
        let server = server_thread.join().expect("join").expect("accept");

        // client → server
        client.send(server_addr, b"ping over h3").expect("send");
        let mut buf = [0u8; 2048];
        let (n, _from) = recv_retry(&server, &mut buf);
        assert_eq!(&buf[..n], b"ping over h3");

        // server → client
        server.send(client.peer(), b"pong").expect("send back");
        let (n, _from) = recv_retry(&client, &mut buf);
        assert_eq!(&buf[..n], b"pong");
    }

    /// recv с несколькими попытками: `WouldBlock` — штатный таймаут, повторяем.
    fn recv_retry(t: &QuicTransport, buf: &mut [u8]) -> (usize, SocketAddr) {
        for _ in 0..50 {
            match t.recv(buf) {
                Ok(pair) => return pair,
                Err(TransportError::WouldBlock) => {}
                Err(e) => panic!("recv error: {e}"),
            }
        }
        panic!("no datagram received within retries")
    }

    #[test]
    fn quic_over_existing_sockets_reuses_mapping() {
        // Доказываем hand-off: оба конца строят QUIC поверх ЗАРАНЕЕ забинденных
        // UDP-сокетов (как переиспользование punched-маппинга), не плодя порты.
        let timeout = Duration::from_millis(200);
        let srv_sock = std::net::UdpSocket::bind(loopback()).expect("srv bind");
        let cli_sock = std::net::UdpSocket::bind(loopback()).expect("cli bind");
        let srv_addr = srv_sock.local_addr().expect("srv addr");

        let listener = QuicListener::from_socket(srv_sock, "example.com").expect("listen");
        let listener_addr = listener.local_addr();
        assert_eq!(listener_addr, srv_addr, "QUIC must reuse the given socket port");
        let server_thread = thread::spawn(move || listener.accept(Duration::from_millis(200)));

        let client = QuicTransport::connect_with_socket(
            cli_sock,
            srv_addr,
            "example.com",
            Duration::from_secs(5),
            timeout,
        )
        .expect("connect");
        let server = server_thread.join().expect("join").expect("accept");

        client.send(srv_addr, b"reuse").expect("send");
        let mut buf = [0u8; 2048];
        let (n, _) = recv_retry(&server, &mut buf);
        assert_eq!(&buf[..n], b"reuse");
    }

    #[test]
    fn connect_to_dead_port_fails_not_panics() {
        // Нет слушателя на этом порту → handshake должен завершиться ошибкой
        // (таймаут idle), не паникой. selector по этому откатился бы.
        let dead: SocketAddr = "127.0.0.1:1".parse().expect("addr");
        let res = QuicTransport::connect(
            loopback(),
            dead,
            "example.com",
            Duration::from_millis(300),
            Duration::from_millis(200),
        );
        assert!(res.is_err(), "expected handshake failure to dead port");
    }
}
