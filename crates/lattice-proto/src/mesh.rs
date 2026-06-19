//! Mesh-протокол coordination-сервера (Фаза 3). Отдельный набор сообщений от
//! Фазы 2 (`control`), чтобы комнатный rendezvous на 2 пиров остался нетронутым.
//!
//! Сервер обслуживает оба режима на одном control-TCP-листенере: первое
//! сообщение решает, room (`ClientMessage::Register`) или mesh
//! (`MeshClientMessage::Hello`). `PROTOCOL_VERSION` инкрементирован до 3.
//!
//! Контракт (см. AGENTS.md «Крипто-модель не меняется»): сервер видит только
//! `network-id` (BLAKE3 от shared-ключа), но не сам ключ. Сводит пиров с
//! одинаковым `network-id`; relay пересылает ciphertext, ключа не имеет.

use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use crate::control::NatType;
use crate::ids::{NetworkId, OverlayIp, PeerId};

/// Сообщения клиент → сервер в mesh-режиме.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MeshClientMessage {
    /// Регистрация в сети. Первое сообщение соединения. `srflx` — внешний
    /// endpoint клиента (`ip:port`, из STUN); `overlay_ip` — self-assigned адрес
    /// в виртуальной сети; `nat` — выведенный тип NAT (для решения punch/relay).
    Hello {
        protocol_version: u32,
        network_id: NetworkId,
        peer_id: PeerId,
        overlay_ip: OverlayIp,
        srflx: String,
        nat: NatType,
    },
    /// Heartbeat: «я ещё жив». Сервер обновляет `last_seen` и не выкидывает пира
    /// при разовой потере (помечает offline только после 3 пропусков ~45с).
    Heartbeat,
    /// Punch к пиру `peer_id` удался — прямой путь установлен. Сервер хранит
    /// per-pair статус для `WebUI` (честный «direct vs relay», не «всё или ничего»).
    PunchOk { peer_id: PeerId },
    /// Punch к пиру `peer_id` провалился — пара работает через relay. Сервер
    /// фиксирует per-pair статус; relay-сессия на сеть уже открыта при join.
    PunchFailed { peer_id: PeerId },
    /// Корректное завершение (shutdown клиента) — сервер шлёт остальным
    /// `PeerLeft`, закрывает записи, не ждёт presence-таймаута.
    Bye,
}

/// Сообщения сервер → клиент в mesh-режиме.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MeshServerMessage {
    /// Регистрация принята. `peers` — текущий список сети (без самого новичка);
    /// `relay_addr`/`session` — relay-сокет сервера и идентификатор сессии на
    /// эту сеть (relay пересылает каждому кроме отправителя). Новичок сразу
    /// получает всё для punch к каждому и fallback на relay.
    Welcome {
        peers: Vec<PeerInfo>,
        relay_addr: String,
        session: u64,
    },
    /// В сеть пришёл новый пир. Все уже зарегистрированные получают это; сам
    /// новичок получает свой `Welcome` вместо `PeerJoined`.
    PeerJoined(PeerInfo),
    /// Пир ушёл (прислал `Bye` / presence-таймаут / кикнут). Остальные
    /// перестают слать ему данные и выкидывают его из своего overlay-реестра.
    PeerLeft { peer_id: PeerId },
    /// Пир сменил endpoint (переподключение после смены сети/NAT) — реестр
    /// обновил запись по `peer_id`, туннель перестраивается, дубль-записи нет.
    PeerUpdated(PeerInfo),
    /// Пира кикнули администратором (WebUI/API). Причина — для отображения.
    Kicked { reason: String },
    /// Сеть закрыта администратором. Все пиры должны переподключиться
    /// (перерегистрация создаст сеть заново, т.к. in-memory).
    NetworkClosed { reason: String },
    /// Регистрация/запрос отклонён (несовпадение версии, коллизия overlay-IP,
    /// отравленный mutex). Текст уходит клиенту, не паника на сервере.
    Error { message: String },
}

/// Описание пира, которым обмениваются сервер и клиенты. Newtype-поля
/// (`PeerId`/`OverlayIp`) не дают спутать со строковыми endpoint'ами. `status`
/// нужен для `WebUI` и для решения punch/relay на клиенте.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub peer_id: PeerId,
    pub overlay_ip: OverlayIp,
    /// Внешний endpoint `ip:port` (srflx из STUN) — куда слать punch / данные.
    pub srflx: String,
    pub nat: NatType,
    /// Per-pair link-статус известен только из отчётов клиента (`PunchOk`/
    /// `PunchFailed`); для свежезарегистрированного пира — `Unknown`.
    pub link: LinkKind,
}

/// Тип link между двумя пирами. Хранится per-pair в реестре и в `WebUI`;
/// `Unknown` — punch ещё не отчёныван (только что join / переподключение).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkKind {
    /// Прямой UDP-путь пробит (punch succeeded).
    Direct,
    /// Пара работает через relay (punch failed или symmetric NAT).
    Relay,
    /// Ещё не известно: ждём отчёта от клиента.
    Unknown,
}

/// Состояние присутствия пира — серверная машина состояний. Переходы:
/// `Online` → (3 пропуска heartbeat) → `Offline` → (`PeerLeft` + удаление);
/// `Online` → (Bye) → `Offline` мгновенно; `Offline` не возвращается в `Online`
/// — переподключение идёт новым `Hello` с тем же `peer_id` (обновление записи).
///
/// Хранится в реестре, виден в `WebUI`. Зачем отдельный enum от `LinkKind`:
/// `PeerStatus` — про наличие пира в сети (presence), `LinkKind` — про путь
/// между двумя пирами (punch/relay). Это разные оси наблюдения.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerStatus {
    /// Активен, heartbeat вовремя.
    Online,
    /// Пропущены heartbeat'ы, но < порога выкидывания — временный лаг, не уход.
    Degraded,
    /// Превышен порог (~45с) — пир помечается ушедшим, остальным уходит
    /// `PeerLeft`, запись удаляется.
    Offline,
}
