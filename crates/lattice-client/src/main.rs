//! lattice-client — точка входа.
//!
//! Phase 1 (статический mesh, не сломан):
//! ```text
//! lattice-client.exe --tap-ip 10.66.0.1/24 --listen 0.0.0.0:51820 \
//!                    --peer <ip>:51820 --key <hex32>
//! ```
//! Phase 2 (NAT traversal через rendezvous):
//! ```text
//! lattice-client.exe --tap-ip 10.66.0.1/24 --listen 0.0.0.0:0 \
//!                    --rendezvous <host>:51821 --room <id> --key <hex32>
//! ```
//! Поднимает tap-windows6, шифрует Ethernet-фреймы ChaCha20-Poly1305 и гоняет
//! их по UDP — напрямую (static / hole punching) либо через relay-сервер.

#![warn(clippy::pedantic)]

mod cli;
mod run;

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::Parser;
use lattice_client::netcfg;
use lattice_client::tap::TapDevice;

use crate::cli::{Cli, Mode};

/// Глобальный флаг shutdown: выставляется console-control-handler'ом по Ctrl+C /
/// закрытию консоли. Воркеры опрашивают его в циклах. static — потому что
/// `SetConsoleCtrlHandler` принимает только plain fn, не замыкание.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let cli = Cli::parse();

    // Контракт edge case: запуск без прав администратора → внятное сообщение.
    if let Err(e) = netcfg::check_admin() {
        eprintln!("error: {e}");
        return ExitCode::from(1);
    }

    let setup = match cli.validate() {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::from(2);
        }
    };

    // TAP-драйвер / адаптер. Контракт edge case: не установлен → подсказка.
    let tap = match TapDevice::open() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(3);
        }
    };
    log::info!("TAP adapter opened: guid={}, name={}", tap.info.guid, tap.info.name);

    // Эффективный MTU зависит от транспорта: QUIC отъедает оверхед поверх UDP,
    // поэтому при forced-QUIC TAP MTU опускается (иначе крупные фреймы тихо
    // дропались бы при инкапсуляции в QUIC DATAGRAM). См. `TransportConfig`.
    let mtu = setup.transport.effective_mtu(netcfg::TAP_MTU);
    if let Err(e) = netcfg::configure_interface(&tap.info.name, setup.ip, setup.prefix_len, mtu) {
        eprintln!("error: interface configuration failed: {e}");
        return ExitCode::from(4);
    }
    log::info!("configured {} with {}/{} MTU={mtu}", tap.info.name, setup.ip, setup.prefix_len);

    // Переустановить media-connected ПОСЛЕ netsh. Смена MTU (`set subinterface`)
    // перезапускает NDIS-минипорт tap-windows6, а рестарт сбрасывает media-status
    // в дефолт (disconnected). Без повторного IOCTL Windows держит адаптер
    // «кабель выдернут»: прячет on-link маршрут, не пишет кадры в адаптер, на
    // пинг overlay-подсети отвечает unreachable без ARP. См. tap::set_media_status.
    if let Err(e) = tap.reassert_media_connected() {
        eprintln!("error: re-asserting TAP media status failed: {e}");
        return ExitCode::from(4);
    }
    log::info!("TAP media re-asserted connected after IP/MTU config");

    // Фаза 4: лог плана транспорта + fail-fast валидация QUIC-конфига (--sni).
    if let Err(e) = run::announce_transport(&setup.transport) {
        eprintln!("error: {e}");
        return ExitCode::from(2);
    }

    // Регистрируем обработчик Ctrl+C до запуска воркеров.
    if !install_ctrl_handler() {
        log::warn!("SetConsoleCtrlHandler failed; graceful shutdown on Ctrl+C disabled");
    }

    let cfg = &setup.transport;
    let code = match &setup.mode {
        Mode::Static(params) => run::run_static(&tap, &setup.crypto, params, cfg, &SHUTDOWN),
        Mode::Dynamic(params) => run::run_dynamic(&tap, &setup.crypto, params, cfg, &SHUTDOWN),
        Mode::Mesh(params) => run::run_mesh(&tap, &setup.crypto, params, cfg, &SHUTDOWN),
    };

    // Drop tap здесь: set_media_status(false) + CloseHandle → линк опускается.
    drop(tap);
    log::info!("shutdown complete");
    code
}

#[cfg(windows)]
extern "system" fn console_ctrl_handler(ctrl_type: u32) -> i32 {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT};
    if matches!(ctrl_type, CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT) {
        log::info!("console ctrl event {ctrl_type}: initiating shutdown");
        SHUTDOWN.store(true, Ordering::Release);
    }
    1 // TRUE — событие обработано.
}

#[cfg(not(windows))]
extern "system" fn console_ctrl_handler(_ctrl_type: u32) -> i32 {
    1
}

/// Регистрирует `console_ctrl_handler` через `tap::install_ctrl_handler` — сам
/// Win32-вызов живёт в модуле `tap` по контракту AGENTS.md.
fn install_ctrl_handler() -> bool {
    lattice_client::tap::install_ctrl_handler(console_ctrl_handler)
}
