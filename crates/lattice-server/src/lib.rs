//! lattice-server: coordination-сервер Lattice (Фаза 3).
//!
//! Раскрыт из минимального rendezvous+relay Фазы 2 в полный control-plane:
//! mesh на N узлов, реестр пиров по сетям (`network-id → peers`), динамический
//! join/leave/heartbeat, relay на сеть, REST API + статический `WebUI`
//! (localhost-only). Фаза 2 (комнатный rendezvous на 2 пиров) не сломана —
//! mesh-режим идёт отдельным набором control-сообщений (`lattice_proto::mesh`),
//! оба режима обслуживаются на одном control-TCP-листенере (dispatch по первому
//! кадру).
//!
//! ## Почему std-потоки, не tokio (контракт сохранён из Фазы 2)
//!
//! `cargo tree -p lattice-server` не должен содержать `windows` crate даже
//! транзитивно (целевой деплой — Linux, проверка на dev-Windows). `tokio` на
//! Windows-хосте тянет `windows-sys` через `mio`; `hyper`/`axum` — поверх tokio.
//! Поэтому HTTP API + `WebUI` реализованы на голом `std::net::TcpListener` с
//! минимальным ручным разбором HTTP/1.1 (`http` модуль), без рантайма. Для
//! нагрузки coordination-сервера (десятки-сотни пиров) блокирующих потоков
//! достаточно; async-рантайм не нужен.
//!
//! ## Каналы
//!
//! - **control** (TCP, `control` + `mesh_control`): room-режим Фазы 2 и
//!   mesh-режим Фазы 3 на одном листенере. Регистрация, список пиров,
//!   heartbeat, punch-отчёты, teardown. Length-delimited JSON (`lattice-proto`).
//! - **relay** (UDP, `relay`): тупой ретранслятор датаплейна. Фаза 3 — одна
//!   сессия на сеть, пересылает каждому кроме отправителя. Видит только
//!   ciphertext — ключа не имеет (E2E сохраняется).
//! - **web** (TCP, `web` + `http`): REST API + статический `WebUI`.
//!   localhost-only без `--web-expose`.
//!
//! ## Persistence
//!
//! In-memory осознанно (см. AGENTS.md). Реестр за `trait Registry` —
//! `InMemoryRegistry` сейчас, SQLite/Redis потом без переписывания callers.

#![warn(clippy::pedantic)]

pub mod control;
pub mod http;
pub mod mesh_control;
pub mod presence;
pub mod registry;
pub mod relay;
pub mod rooms;
pub mod web;

/// Length-delimited фрейминг control-канала. Реэкспорт из `lattice-proto` —
/// единый формат с клиентом, чтобы контракт не разъезжался.
pub mod wire {
    pub use lattice_proto::framing::{read_frame, write_frame, MAX_FRAME_LEN};
}

pub use lattice_proto::PROTOCOL_VERSION;

use thiserror::Error;

/// Ошибки старта/работы сервера. Сетевые ошибки несут контекст (что биндили),
/// чтобы оператор VPS видел причину, а не голый код.
#[derive(Debug, Error)]
pub enum ServerError {
    #[error("failed to bind control listener on {addr}: {source}")]
    ControlBind {
        addr: String,
        source: std::io::Error,
    },
    #[error("failed to bind relay socket on {addr}: {source}")]
    RelayBind {
        addr: String,
        source: std::io::Error,
    },
    #[error("failed to bind web listener on {addr}: {source}")]
    WebBind {
        addr: String,
        source: std::io::Error,
    },
}
