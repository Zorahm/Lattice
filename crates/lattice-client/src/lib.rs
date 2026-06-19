//! lattice-client — Windows-only клиент Lattice (Фаза 1, `PoC`).
//!
//! Поднимает виртуальный L2-адаптер `tap-windows6`, шифрует Ethernet-фреймы
//! `ChaCha20-Poly1305` и гоняет их по UDP между пирами. Подробности: SPEC.md,
//! AGENTS.md. Бинарь — `src/main.rs`, вся логика в библиотечных модулях:
//!
//! - `crypto` — AEAD-обёртка с newtype-ключом/nonce.
//! - `tap` — FFI к `tap-windows6` (единственное место с `unsafe`).
//! - `transport` — UDP-реализация `trait Transport` (сменяемый слой для Фазы 4).
//! - `peers` — статический и динамический реестр пиров за `trait Discovery`.
//! - `netcfg` — назначение IP/MTU через `netsh` + проверка прав администратора.
//!
//! Фаза 2 (NAT traversal) — кроссплатформенные сетевые модули:
//! - `stun` — внешний (srflx) endpoint + эвристика типа NAT.
//! - `signaling` — control-канал к rendezvous-серверу (TCP).
//! - `punch` — UDP hole punching + keepalive.
//! - `relay` — relay-транспорт (fallback при провале punch) за `trait Transport`.
//! - `dynamic` — установление соединения (STUN → rendezvous → punch/relay).
//! - `session` — датаплейн-циклы + watchdog, общие для static/direct/relay.
//!
//! Фаза 3 (coordination-сервер, mesh) — кроссплатформенные сетевые модули:
//! - `network_id` — `network-id = BLAKE3(shared-key)` (ключ не покидает клиент).
//! - `mesh` — mesh Discovery/установление: STUN → coordination-сервер →
//!   punch-per-peer → direct/relay; heartbeat, reconnect, Presence-апдейты.
//!
//! Фаза 4 (обфускация транспорта) — подмодули `transport` (только клиент):
//! - `transport::quic`/`quic_tls` — QUIC поверх UDP как маскировка под HTTP/3
//!   (ALPN h3, настраиваемый SNI), за тем же `trait Transport`. Внешний слой —
//!   ради маскировки, внутренний `ChaCha` — ради E2E (двойное шифрование осознанно).
//! - `transport::obfs` — padding длин + timing jitter (конфигурируемы, дефолт ВЫКЛ).
//! - `transport::selector` — машина выбора auto/udp/quic с эскалацией и backoff.
//!
//! Честная планка Фазы 4: маскировка против сигнатуры известных VPN и пассивной
//! эвристики «непонятный UDP»; против активного пробинга не тестировалось и не
//! заявляется. Гарантий обхода конкретного DPI нет (см. AGENTS.md).

#![cfg_attr(not(windows), allow(unused))]
#![warn(clippy::pedantic)]

pub mod crypto;
pub mod dynamic;
pub mod mesh;
pub mod mesh_session;
pub mod netcfg;
pub mod network_id;
pub mod peers;
pub mod punch;
pub mod relay;
pub mod session;
pub mod signaling;
pub mod stun;
pub mod tap;
pub mod transport;
