// lattice-proto: общие типы протокола между lattice-client и lattice-server.
//
// Контракт (см. AGENTS.md / SPEC.md): здесь НЕ должно быть платформенных
// зависимостей — только serde. Крейт собирается без `std` (no_std + alloc),
// чтобы его можно было переиспользовать из любого таргета. С Фазы 2 здесь живут
// сообщения rendezvous-сигналинга (control-канал клиент↔сервер) и формат
// relay-обёртки датаплейна. Фаза 3 добавляет mesh-сообщения (`mesh`) и
// newtype-идентификаторы (`ids`). Сериализацию (serde_json для control, ручные
// байты для relay) делают client/server — proto держит только определения,
// чтобы оставаться no_std и не тянуть serde_json/std.

// Крейт no_std по умолчанию; фича `std` включает `framing` (length-delimited
// JSON поверх `std::io`) — её используют client/server, у которых std есть.
// Сам формат кадра (4-байтный BE-префикс длины) — единый источник истины для
// обеих сторон, чтобы контракт не разъезжался.
#![cfg_attr(not(feature = "std"), no_std)]
#![warn(clippy::pedantic)]

extern crate alloc;

pub mod control;
#[cfg(feature = "std")]
pub mod framing;
pub mod ids;
pub mod mesh;
pub mod relay;

pub use control::{ClientMessage, NatType, RoomId, ServerMessage, StartMode};
pub use ids::{NetworkId, OverlayIp, PeerId, NETWORK_ID_HEX_LEN};
pub use mesh::{LinkKind, MeshClientMessage, MeshServerMessage, PeerInfo, PeerStatus};

use alloc::string::String;
use serde::{Deserialize, Serialize};

/// Версия wire-протокола coordination-сервера. Инкрементируется при несовместимых
/// изменениях control-сообщений. Клиент шлёт её в `Register`/`Hello`; сервер с
/// другой версией обязан отклонить (`ServerMessage::Error`/`MeshServerMessage::Error`),
/// чтобы пиры со старым протоколом не «договорились» наполовину. PoC-датаплейн
/// (UDP-датаграммы с AEAD) этой версии не использует — он самописный и не
/// сериализуется serde.
///
/// История: 1 — Фаза 1 (плейсхолдер); 2 — Фаза 2 (room rendezvous); 3 — Фаза 3
/// (mesh-сообщения, отдельный набор от room-протокола, обе ветки на одном
/// листенере).
pub const PROTOCOL_VERSION: u32 = 3;

/// Описание пира, которым обмениваются клиент и coordination-сервер (Фаза 3).
/// Endpoint хранится строкой, чтобы не тащить платформенно-зависимый
/// `SocketAddr` в shared-крейт и оставить его no_std-friendly (парсинг в
/// `SocketAddr` — на стороне client/server, где есть `std::net`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerDescriptor {
    pub id: String,
    pub endpoint: String,
}
