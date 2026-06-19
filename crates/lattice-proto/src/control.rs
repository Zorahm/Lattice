//! Control-канал rendezvous (клиент↔сервер). Сериализуется как
//! length-delimited JSON (`serde_json` в client/server — proto только определяет
//! типы, чтобы остаться `no_std`).
//!
//! Почему control-канал отдельный (TCP), а не поверх датаплейн-UDP:
//! регистрация → матч пиров → синхронный go-сигнал требуют надёжной
//! упорядоченной доставки. Реализовывать ретрансмиты поверх UDP — лишняя
//! сложность; TCP даёт это даром. Датаплейн и relay при этом остаются UDP
//! (см. `relay`) — control и data не делят сокет, демультиплексировать STUN /
//! punch / data на одном порту не нужно.

use alloc::string::String;
use serde::{Deserialize, Serialize};

/// Идентификатор «комнаты» — общий секрет/метка, по которой сервер сводит двух
/// пиров. Newtype, чтобы не путать со случайной строкой (endpoint, id и т.п.).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoomId(String);

impl RoomId {
    #[must_use]
    pub fn new(id: String) -> Self {
        Self(id)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Эвристический тип NAT, выведенный клиентом по сравнению srflx-маппингов на
/// разные STUN-таргеты (см. client `stun.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NatType {
    /// Не удалось определить (STUN недоступен / один таргет ответил). Сервер
    /// трактует как «попробовать punch, но быть готовым к relay».
    Unknown,
    /// Маппинг не зависит от назначения (full-cone / restricted / port-
    /// restricted): внешний порт одинаков для разных таргетов → punch реален,
    /// т.к. srflx, увиденный через STUN, совпадёт с тем, что увидит пир.
    EndpointIndependent,
    /// Симметричный NAT: на каждый новый destination — новый внешний порт.
    /// srflx из STUN бесполезен для пира → punch почти наверняка провалится,
    /// сервер сразу назначает relay (не тратя секунды на обречённый punch).
    Symmetric,
}

/// Как стартовать сессию — решает сервер, зная NAT обоих пиров.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StartMode {
    /// Пробовать hole punching: оба шлют bursts по go-сигналу.
    Punch,
    /// Сразу relay (хотя бы один пир symmetric → punch обречён).
    Relay,
}

/// Сообщения клиент → сервер.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Регистрация в комнате. `srflx` — внешний endpoint клиента (из STUN),
    /// строкой `ip:port`. `nat` — выведенный тип NAT.
    Register {
        protocol_version: u32,
        room: RoomId,
        srflx: String,
        nat: NatType,
    },
    /// Punch не сошёлся за таймаут — клиент просит сервер перевести сессию в
    /// relay. Сервер отвечает, что relay уже доступен (`relay_addr`/`session`
    /// были присланы в `Start`), либо ошибкой.
    PunchFailed,
    /// Информационно: punch удался (для логов сервера, не обязателен).
    PunchOk,
    /// Корректное завершение сессии — сервер освобождает комнату/relay-сессию,
    /// чтобы второй пир получил `PeerGone`, а не висел.
    Bye,
}

/// Сообщения сервер → клиент.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Регистрация принята, ждём второго пира.
    Registered,
    /// Второй пир найден. Несёт всё необходимое сразу, чтобы fallback на relay
    /// не требовал лишнего round-trip: `relay_addr`/`session` валидны всегда,
    /// даже если `mode == Punch` (клиент уйдёт туда сам по таймауту).
    Start {
        /// Внешний endpoint пира (`ip:port`), куда слать punch / данные.
        peer_endpoint: String,
        peer_nat: NatType,
        mode: StartMode,
        /// UDP-адрес relay-сокета сервера.
        relay_addr: String,
        /// Идентификатор relay-сессии (общий для обоих пиров комнаты).
        session: u64,
    },
    /// Пир отвалился (закрыл control-соединение / прислал `Bye`). Клиент
    /// корректно завершает сессию, а не зависает.
    PeerGone,
    /// Регистрация/запрос отклонён (несовпадение версии, комната занята, и т.п.).
    Error { message: String },
}
