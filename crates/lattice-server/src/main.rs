//! lattice-server — точка входа coordination-сервера (Фаза 3).
//!
//! ```text
//! lattice-server --control 0.0.0.0:51821 --relay 0.0.0.0:51822 \
//!                --relay-advertise <публичный ip>:51822 \
//!                --web-bind 127.0.0.1:51823
//! ```
//! Обслуживает room-режим Фазы 2 (2 пира на комнату) и mesh-режим Фазы 3
//! (N пиров на сеть) на одном control-TCP-листенере. Relay — UDP, сессия на
//! сеть. WebUI/API — localhost-only по умолчанию; `--web-expose` + `--web-bind
//! 0.0.0.0` торчит наружу. Кроссплатформенный, деплой — Linux VPS.

#![warn(clippy::pedantic)]

use std::net::{TcpListener, UdpSocket};
use std::process::ExitCode;
use std::thread;

use clap::Parser;
use lattice_server::presence;
use lattice_server::registry::InMemoryRegistry;
use lattice_server::relay::RelayTable;
use lattice_server::rooms::Rooms;
use lattice_server::{control, relay, web, ServerError, PROTOCOL_VERSION};

#[derive(Parser, Debug)]
#[command(
    name = "lattice-server",
    version,
    about = "Lattice Phase 3 coordination server (room + mesh, relay, web)"
)]
struct Cli {
    /// TCP-адрес control-канала (room + mesh на одном порту).
    #[arg(long, value_name = "ADDR", default_value = "0.0.0.0:51821")]
    control: String,
    /// UDP-адрес relay-сокета (ретрансляция датаплейна).
    #[arg(long, value_name = "ADDR", default_value = "0.0.0.0:51822")]
    relay: String,
    /// Адрес relay, сообщаемый клиентам (`ip:port`). На VPS за 0.0.0.0 укажи
    /// публичный IP — иначе клиенты не достучатся. По умолчанию = `--relay`.
    #[arg(long, value_name = "ADDR")]
    relay_advertise: Option<String>,
    /// TCP-адрес WebUI/API. По умолчанию localhost-only.
    #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:51823")]
    web_bind: String,
    /// Разрешить внешние запросы к `WebUI` (иначе не-localhost → 403). При
    /// включении обычно `--web-bind 0.0.0.0`.
    #[arg(long, default_value_t = false)]
    web_expose: bool,
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Внятная ошибка старта, не паника (контракт edge case).
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn run(cli: &Cli) -> Result<(), ServerError> {
    log::info!("lattice-server (protocol v{PROTOCOL_VERSION}) starting");

    let relay_socket = UdpSocket::bind(&cli.relay).map_err(|source| ServerError::RelayBind {
        addr: cli.relay.clone(),
        source,
    })?;
    let relay_advertise = cli.relay_advertise.clone().unwrap_or_else(|| cli.relay.clone());
    log::info!("relay bound on {}, advertised as {relay_advertise}", cli.relay);

    let web_listener = TcpListener::bind(&cli.web_bind).map_err(|source| ServerError::WebBind {
        addr: cli.web_bind.clone(),
        source,
    })?;

    let control_listener = TcpListener::bind(&cli.control)
        .map_err(|source| ServerError::ControlBind {
            addr: cli.control.clone(),
            source,
        })?;

    let table = RelayTable::new();
    // Rooms (Фаза 2) и Registry (Фаза 3) делят один relay-таблица — сессии
    // уникальны (каждая выделяет свой u64), конфликтов нет.
    let rooms = Rooms::new(table.clone(), relay_advertise.clone());
    let registry = InMemoryRegistry::new(table.clone(), relay_advertise.clone());

    // Relay крутится в своём потоке: блокирующий recv на UDP, независим от
    // control/web-accept. Падать ему нельзя — serve() сам не возвращается.
    let relay_table = table.clone();
    let relay_handle = thread::Builder::new()
        .name("relay".into())
        .spawn(move || relay::serve(&relay_socket, &relay_table))
        .map_err(|e| ServerError::RelayBind {
            addr: cli.relay.clone(),
            source: std::io::Error::other(e.to_string()),
        })?;

    // Presence-чистка: отдельный поток, периодически зовёт
    // `registry.presence_sweep` и удаляет протухших пиров (3 пропуска heartbeat).
    let presence_registry = registry.clone();
    let presence_handle = thread::Builder::new()
        .name("presence".into())
        .spawn(move || presence::serve(&presence_registry, presence::DEFAULT_HEARTBEAT_INTERVAL))
        .map_err(|e| ServerError::ControlBind {
            addr: "presence".to_string(),
            source: std::io::Error::other(e.to_string()),
        })?;

    // WebUI/API в своём потоке — блокирующий accept, localhost-only gate
    // внутри `web::serve`. `web_expose` — `bool` (`Copy`), выносим из `cli`
    // (заимствовать `cli` в `'static`-потоке нельзя).
    let web_registry = registry.clone();
    let web_expose = cli.web_expose;
    let web_handle = thread::Builder::new()
        .name("web".into())
        .spawn(move || web::serve(web_listener, &web_registry, web_expose))
        .map_err(|e| ServerError::WebBind {
            addr: cli.web_bind.clone(),
            source: std::io::Error::other(e.to_string()),
        })?;

    // Control-accept в основном потоке — блокирующий цикл до завершения процесса.
    // Обслуживает room (Фаза 2) и mesh (Фаза 3) на одном листенере.
    control::serve(&control_listener, &rooms, &registry);

    // serve() не возвращается штатно; join здесь — на случай будущего graceful
    // shutdown, чтобы фоновые потоки не остались висеть.
    let _ = relay_handle.join();
    let _ = presence_handle.join();
    let _ = web_handle.join();
    Ok(())
}
