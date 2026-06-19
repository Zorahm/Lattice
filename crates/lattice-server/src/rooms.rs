//! Реестр комнат: сводит ровно двух пиров по `RoomId`.
//!
//! Минимализм Фазы 2 (SPEC): максимум 2 участника на комнату, без персистентности
//! и без mesh > 2 — это Фаза 3. Когда во комнату приходит второй пир, сервер
//! решает режим старта (punch/relay) по NAT обоих и шлёт обоим `Start`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use lattice_proto::{NatType, RoomId, ServerMessage, StartMode};

use crate::relay::RelayTable;

/// Участник комнаты. `tx` — в writer-поток его control-соединения (см.
/// `control`): так сервер пушит `Start`/`PeerGone` асинхронно, не блокируясь на
/// сокете под локом реестра.
pub struct Member {
    pub conn_id: u64,
    pub srflx: String,
    pub nat: NatType,
    pub tx: Sender<ServerMessage>,
}

struct Room {
    /// Общий для обоих пиров id relay-сессии (выдан при создании комнаты).
    session: u64,
    members: Vec<Member>,
}

/// Итог попытки регистрации.
pub enum RegisterOutcome {
    /// Принят; ждём второго пира (или пара уже сведена — `Start` разослан).
    Accepted,
    /// Отклонён (комната занята / версия протокола) — текст ушёл клиенту.
    Rejected,
}

/// Общий реестр. Клонируется дёшево (всё внутри `Arc`); по копии получает
/// каждый control-поток.
#[derive(Clone)]
pub struct Rooms {
    inner: Arc<Mutex<HashMap<RoomId, Room>>>,
    relay: RelayTable,
    /// Адрес relay-сокета, который сервер сообщает клиентам (`ip:port`).
    relay_advertise: String,
    session_seq: Arc<AtomicU64>,
}

impl Rooms {
    #[must_use]
    pub fn new(relay: RelayTable, relay_advertise: String) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            relay,
            relay_advertise,
            // Старт с 1: 0 зарезервирован как «нет сессии» в логах.
            session_seq: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Зарегистрировать участника в комнате. При появлении второго — рассылает
    /// `Start` обоим и открывает relay-сессию. Сообщения уходят через `tx`
    /// участников, поэтому ошибки сокета сюда не пробрасываются.
    #[must_use]
    pub fn register(&self, room_id: &RoomId, member: Member) -> RegisterOutcome {
        let Ok(mut rooms) = self.inner.lock() else {
            // Mutex отравлен паникой другого потока — не продолжаем вслепую.
            let _ = member.tx.send(ServerMessage::Error {
                message: "server registry unavailable".into(),
            });
            return RegisterOutcome::Rejected;
        };

        let room = rooms.entry(room_id.clone()).or_insert_with(|| Room {
            session: self.session_seq.fetch_add(1, Ordering::Relaxed),
            members: Vec::new(),
        });

        if room.members.len() >= 2 {
            // Комната занята: Фаза 2 — строго 2 пира. Третьего отклоняем внятно.
            let _ = member.tx.send(ServerMessage::Error {
                message: "room is full (phase 2 supports exactly two peers)".into(),
            });
            // Пустую только что созданную комнату не оставляем — но здесь она не
            // пустая (>=2), так что просто выходим.
            return RegisterOutcome::Rejected;
        }

        room.members.push(member);
        if let Some(m) = room.members.last() {
            let _ = m.tx.send(ServerMessage::Registered);
        }

        if room.members.len() == 2 {
            self.relay.open(room.session);
            Self::announce_pair(room, &self.relay_advertise);
        }

        RegisterOutcome::Accepted
    }

    /// Разослать `Start` обоим участникам. Каждому уходит endpoint/NAT ДРУГОГО.
    fn announce_pair(room: &Room, relay_advertise: &str) {
        let mode = decide_mode(room.members[0].nat, room.members[1].nat);
        log::info!(
            "matched pair in room (session {}): {} <-> {}, mode={:?}",
            room.session,
            room.members[0].srflx,
            room.members[1].srflx,
            mode
        );
        for (idx, m) in room.members.iter().enumerate() {
            let peer = &room.members[1 - idx];
            let _ = m.tx.send(ServerMessage::Start {
                peer_endpoint: peer.srflx.clone(),
                peer_nat: peer.nat,
                mode,
                relay_addr: relay_advertise.to_string(),
                session: room.session,
            });
        }
    }

    /// Участник ушёл (disconnect / `Bye`). Сносим комнату целиком: 2-пировая
    /// сессия мертва, как только один ушёл. Второму шлём `PeerGone`, relay-
    /// сессию закрываем — чтобы он не завис и не утекали ресурсы.
    pub fn leave(&self, room_id: &RoomId, conn_id: u64) {
        let Ok(mut rooms) = self.inner.lock() else {
            return;
        };
        let Some(room) = rooms.get_mut(room_id) else {
            return;
        };
        let session = room.session;
        for m in &room.members {
            if m.conn_id != conn_id {
                let _ = m.tx.send(ServerMessage::PeerGone);
            }
        }
        rooms.remove(room_id);
        self.relay.close(session);
        log::info!("room torn down (session {session}), peer {conn_id} left");
    }
}

/// Решение punch vs relay по NAT обоих пиров. Любой `Symmetric` → relay: его
/// srflx, увиденный через STUN, не совпадёт с маппингом в сторону пира, и punch
/// почти наверняка провалится — не тратим на него секунды. Иначе пробуем punch
/// (`Unknown` трактуем оптимистично — STUN мог не ответить на cone-NAT).
fn decide_mode(a: NatType, b: NatType) -> StartMode {
    if a == NatType::Symmetric || b == NatType::Symmetric {
        StartMode::Relay
    } else {
        StartMode::Punch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symmetric_forces_relay() {
        assert_eq!(
            decide_mode(NatType::Symmetric, NatType::EndpointIndependent),
            StartMode::Relay
        );
        assert_eq!(
            decide_mode(NatType::EndpointIndependent, NatType::Symmetric),
            StartMode::Relay
        );
    }

    #[test]
    fn cone_pair_punches() {
        assert_eq!(
            decide_mode(NatType::EndpointIndependent, NatType::EndpointIndependent),
            StartMode::Punch
        );
    }

    #[test]
    fn unknown_is_optimistic() {
        assert_eq!(
            decide_mode(NatType::Unknown, NatType::Unknown),
            StartMode::Punch
        );
    }
}
