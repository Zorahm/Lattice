//! Реестр пиров. Фаза 1 — статический список из CLI. Фаза 2 подменит на
//! STUN / hole-punching за тем же `trait Discovery`, без переписывания
//! crypto/tap.
//!
//! Контракт (AGENTS.md «Сменяемый транспорт»): discovery оторван от датаплейна
//! и не знает про crypto/TAP. От него только одно — выдать актуальный список
//! `SocketAddr` пиров для broadcast-рассылки в tap→udp воркере.

use std::net::SocketAddr;

use thiserror::Error;

/// Ошибка discovery. На Фазе 1 практически невозможна (статический список),
/// на Фазе 2 здесь появится STUN-failure / нет relay — поэтому тип заведён
/// сразу, чтобы не менять сигнатуру трейта позже.
#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("no peers configured")]
    NoPeers,
    #[error("discovery backend failure: {0}")]
    Backend(String),
}

/// Сменяемое discovery пиров. Фаза 1: `StaticPeers`. Фаза 2: STUN-based.
pub trait Discovery {
    /// Текущий снимок списка пиров. Возвращает slice, чтобы воркер tap→udp
    /// мог итерировать без аллокации на каждый фрейм.
    ///
    /// # Errors
    ///
    /// `NoPeers` — список пуст; `Backend` — провал backend discovery (Фаза 2).
    fn peers(&self) -> Result<&[SocketAddr], DiscoveryError>;
}

/// Статический список пиров из аргументов командной строки. Неизменен после
/// старта — перечитывать неоткуда.
pub struct StaticPeers {
    peers: Vec<SocketAddr>,
}

impl StaticPeers {
    /// Собрать из списка адресов. Дубликаты выкидываем, чтобы не слать
    /// дважды одному пиpy при broadcast-рассылке.
    #[must_use]
    pub fn new(mut peers: Vec<SocketAddr>) -> Self {
        peers.sort();
        peers.dedup();
        Self { peers }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }
}

impl Discovery for StaticPeers {
    fn peers(&self) -> Result<&[SocketAddr], DiscoveryError> {
        if self.peers.is_empty() {
            return Err(DiscoveryError::NoPeers);
        }
        Ok(&self.peers)
    }
}

/// Discovery Фазы 2: единственный peer-endpoint, разрешённый в рантайме через
/// STUN + rendezvous + punch (см. `dynamic`/`signaling`/`punch`). За тем же
/// `trait Discovery`, что и статика — датаплейн-циклы не отличают режимы.
///
/// В direct-режиме это подтверждённый адрес пира; в relay-режиме — адрес
/// relay-сокета сервера (датаграммы уходят туда, сервер их ретранслирует).
pub struct DynamicPeers {
    target: [SocketAddr; 1],
}

impl DynamicPeers {
    #[must_use]
    pub fn new(target: SocketAddr) -> Self {
        Self { target: [target] }
    }
}

impl Discovery for DynamicPeers {
    fn peers(&self) -> Result<&[SocketAddr], DiscoveryError> {
        Ok(&self.target)
    }
}
