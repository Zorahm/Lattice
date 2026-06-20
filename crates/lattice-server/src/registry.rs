//! Реестр сетей и пиров Фазы 3: `network-id → { peers, relay-session }`.
//!
//! За `trait Registry`, чтобы in-memory сейчас, а SQLite/Redis потом — без
//! переписывания callers (control, presence, web). `InMemoryRegistry` —
//! единственная реализация на Фазе 3.
//!
//! ## Почему in-memory осознанно
//!
//! Состояние живёт в RAM и теряется при рестарте сервера. Это ОК: клиенты при
//! потере связи переподключаются по таймауту heartbeat и перерегистрируются
//! (`Hello`), реестр восстанавливается из живых клиентов. Персистентность
//! добавила бы сложность (сброс на диск / WAL) без выгоды для PoC/MVP-overlay.
//!
//! ## Почему `RwLock`, не каналы (actor)
//!
//! Несколько читателей: `presence`-чистка сканирует `last_seen`, `web`-snapshot
//! читает весь реестр, `mesh_control` читает при join. Писатели редки
//! (join/leave/heartbeat). `RwLock` даёт параллельный read и короткий write без
//! отдельного потока-обработчика (actor потребовал бы mpsc + выделенный поток,
//! что усложняет shutdown). Для нагрузки coordination-сервера (десятки-сотни
//! пиров, не миллионы) contention минимальный. `Mutex` Фазы 2 работал, но
//! `web`-чтение всей структуры под exclusive-lock блокировало бы join'ы —
//! `RwLock` развязывает читателей.
//!
//! ## Рассылка апдейтов
//!
//! `PeerEntry` хранит `Sender<MeshServerMessage>` (writer-поток control-канала
//! пира). Операции (`join`/`leave`/`kick`/`close`) рассылают апдейты сами —
//! реестр единственный, кто видит согласованное состояние сети под локом, и
//! отправка из-под lock'а (через `send`, не блокирующий) не рискует дедлоком
//! (канал mpsc неблокирующий). `send` ошибки логируем — пир мог только что
//! отключиться, его writer уже мёртв; не падаем.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use lattice_proto::mesh::{LinkKind, MeshServerMessage, PeerInfo, PeerStatus};
use lattice_proto::{NatType, NetworkId, OverlayIp, PeerId};

use crate::relay::RelayTable;

/// Порог presence: 3 пропуска heartbeat (~45с при интервале 15с) → пир offline.
/// Один пропуск — временный лаг (UDP-потеря/TCP-лаг), не выкидываем. См.
/// `presence.rs`.
pub const HEARTBEAT_DEGRADED_AFTER: usize = 1;
pub const HEARTBEAT_OFFLINE_AFTER: usize = 3;

/// Запрос на регистрацию пира в сети. Вынесен в структуру, чтобы `join` не
/// нёс 8 позиционных аргументов (`clippy::too_many_arguments`) и callers
/// читались по полям. Все поля owned — `join` потребляет их (insert без clone).
pub struct JoinRequest {
    pub network_id: NetworkId,
    pub peer_id: PeerId,
    pub overlay_ip: OverlayIp,
    pub srflx: String,
    pub nat: NatType,
    /// LAN-local endpoint пира (`ip:port`) из `Hello` — для прямого пути между
    /// пирами за одним публичным IP. Прозрачно ретранслируется в `PeerInfo`.
    pub local_addr: Option<String>,
    /// TCP source-адрес control-соединения — для `WebUI` (диагностика).
    pub control_addr: SocketAddr,
    pub tx: Sender<MeshServerMessage>,
}

/// Запись о пире в сети. `tx` — в writer-поток его control-соединения; реестр
/// через него пушит `PeerJoined`/`PeerLeft`/`PeerUpdated` асинхронно, не
/// блокируясь на сокете под локом.
pub struct PeerEntry {
    pub peer_id: PeerId,
    pub overlay_ip: OverlayIp,
    pub srflx: String,
    pub nat: NatType,
    /// LAN-local endpoint пира (`ip:port`) — ретранслируется в `PeerInfo`.
    pub local_addr: Option<String>,
    /// TCP source-адрес control-соединения — для `WebUI` (диагностика).
    pub control_addr: SocketAddr,
    pub last_seen: Instant,
    pub missed_heartbeats: usize,
    pub status: PeerStatus,
    /// Per-pair link к каждому другому пиру сети. Заполняется из отчётов
    /// клиента (`PunchOk`/`PunchFailed`); `Unknown` до первого отчёта.
    pub links: HashMap<PeerId, LinkKind>,
    tx: Sender<MeshServerMessage>,
}

impl PeerEntry {
    fn to_info(&self, link: LinkKind) -> PeerInfo {
        PeerInfo {
            peer_id: self.peer_id.clone(),
            overlay_ip: self.overlay_ip.clone(),
            srflx: self.srflx.clone(),
            nat: self.nat,
            local_addr: self.local_addr.clone(),
            link,
        }
    }
}

/// Сеть: `network-id` + relay-сессия + пиры. Ключи `PeerId` — в рамках сети.
/// Поле названо `id` (не `network_id`), чтобы не дублировать имя типа
/// (`clippy::struct_field_names`).
struct Network {
    id: NetworkId,
    relay_session: u64,
    peers: HashMap<PeerId, PeerEntry>,
}

impl Network {
    fn broadcast(&self, msg: &MeshServerMessage, except: Option<&PeerId>) {
        for (pid, p) in &self.peers {
            if except == Some(pid) {
                continue;
            }
            if p.tx.send(msg.clone()).is_err() {
                // Пир уже отключился — его writer мёртв. `presence`/`leave`
                // уберёт запись; здесь просто не падаем.
                log::debug!("registry: broadcast to {pid} dropped (writer gone)");
            }
        }
    }
}

/// Read-only снимок пира для `WebUI`/тестов (без `tx`, без `Instant`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct PeerSnapshot {
    pub peer_id: PeerId,
    pub overlay_ip: OverlayIp,
    pub srflx: String,
    pub nat: NatType,
    pub control_addr: String,
    pub status: PeerStatus,
    pub links: Vec<(PeerId, LinkKind)>,
}

/// Read-only снимок сети.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NetworkSnapshot {
    pub network_id: NetworkId,
    pub relay_session: u64,
    pub peers: Vec<PeerSnapshot>,
}

/// Данные, которые `join` возвращает для `Welcome` новичку.
#[derive(Debug)]
pub struct Welcome {
    pub peers: Vec<PeerInfo>,
    pub relay_addr: String,
    pub session: u64,
}

/// Ошибка реестра. Все — рантаймные, не паника; сервер отвечает `Error`.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// Два пира в одной сети с одинаковым `overlay-ip` — детект коллизии,
    /// сервер сигналит второму, не молчаливый конфликт в датаплейне.
    #[error("overlay-ip {0} already taken in this network by another peer")]
    OverlayIpCollision(OverlayIp),
    /// `network-id` не найден при операции, которая его ожидает (kick/leave
    /// после удаления сети).
    #[error("network not found")]
    NetworkNotFound,
    /// `peer-id` не найден в сети.
    #[error("peer not found in network")]
    PeerNotFound,
}

/// Сменяемый бэкенд реестра. `InMemoryRegistry` — текущая реализация; `SQLite`
/// потом добавляется как ещё одна реализация, callers не меняются.
pub trait Registry: Send + Sync {
    /// Регистрация пира в сети. При коллизии `overlay-ip` — `Err` (сервер шлёт
    /// `Error`). При успехе: открывает relay-сессию (если сеть новая), рассылает
    /// `PeerJoined` остальным, возвращает данные для `Welcome` новичку. При
    /// переподключении с тем же `peer-id` — обновляет endpoint/srflx/nat,
    /// рассылает `PeerUpdated`, не дубль-запись.
    ///
    /// # Errors
    ///
    /// `OverlayIpCollision` — overlay-IP уже занят в этой сети другим пиром.
    fn join(&self, req: JoinRequest) -> Result<Welcome, RegistryError>;

    /// Пир ушёл (`Bye`/disconnect). Рассылает `PeerLeft`, удаляет запись. Если
    /// был последним — закрывает relay-сессию и удаляет сеть.
    fn leave(&self, net: &NetworkId, peer_id: &PeerId);

    /// Heartbeat: обновляем `last_seen`, сбрасываем счётчик пропусков, статус →
    /// `Online` (если был `Degraded`). Не рассылает ничего — presence-inner.
    fn heartbeat(&self, net: &NetworkId, peer_id: &PeerId);

    /// Отчёт punch: фиксируем per-pair link (`Direct`/`Relay`) для `WebUI`.
    fn punch_report(&self, net: &NetworkId, from: &PeerId, to: &PeerId, kind: LinkKind);

    /// Кик пира администратором (`WebUI`/API). Шлёт `Kicked` самому, `PeerLeft`
    /// остальным, удаляет запись.
    fn kick(&self, net: &NetworkId, peer_id: &PeerId, reason: &str);

    /// Закрыть сеть администратором. Шлёт `NetworkClosed` всем, закрывает relay,
    /// удаляет сеть. Клиенты переподключатся (in-memory — сеть создастся заново).
    fn close_network(&self, net: &NetworkId, reason: &str);

    /// Read-only snapshot всех сетей — для `WebUI`/API.
    fn snapshot(&self) -> Vec<NetworkSnapshot>;

    /// Presence-чистка: помечает `Degraded`/`Offline` по `last_seen`, удаляет
    /// протухших (рассылает `PeerLeft`, закрывает relay пустых сетей). Возвращает
    /// список удалённых `(network, peer)` для логов presence-потока. Вся мутация
    /// под одним write-lock — вызывающий не держит lock и не вызывает `leave`
    /// отдельно (это избегает двойного лока и race с heartbeat).
    fn presence_sweep(
        &self,
        heartbeat_interval: Duration,
        offline_after: usize,
    ) -> Vec<(NetworkId, PeerId)>;
}

/// In-memory реализация: `Arc<RwLock<HashMap<NetworkId, Network>>>`. Клонируется
/// дёшево (всё внутри `Arc`); копию получают `mesh_control`/`presence`/`web`.
#[derive(Clone)]
pub struct InMemoryRegistry {
    inner: Arc<RwLock<HashMap<NetworkId, Network>>>,
    relay: RelayTable,
    relay_advertise: String,
    session_seq: Arc<AtomicU64>,
}

impl InMemoryRegistry {
    #[must_use]
    pub fn new(relay: RelayTable, relay_advertise: String) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            relay,
            relay_advertise,
            // Старт с 1: 0 зарезервирован как «нет сессии» в логах.
            session_seq: Arc::new(AtomicU64::new(1)),
        }
    }

    fn write(&self) -> Result<std::sync::RwLockWriteGuard<'_, HashMap<NetworkId, Network>>, RegistryError> {
        self.inner.write().map_err(|_| {
            // Отравленный lock (паника holder'а) — не продолжаем вслепую.
            RegistryError::NetworkNotFound
        })
    }

    fn session_for_new_network(&self) -> u64 {
        self.session_seq.fetch_add(1, Ordering::Relaxed)
    }
}

impl Registry for InMemoryRegistry {
    fn join(&self, req: JoinRequest) -> Result<Welcome, RegistryError> {
        let JoinRequest {
            network_id: net,
            peer_id,
            overlay_ip,
            srflx,
            nat,
            local_addr,
            control_addr,
            tx,
        } = req;
        let mut nets = self.write()?;
        // Коллизия overlay-IP: два пира в одной сети с одинаковым self-assigned
        // адресом → сервер сигналит, не молчаливый конфликт в датаплейне.
        if let Some(net_entry) = nets.get(&net) {
            for p in net_entry.peers.values() {
                if p.overlay_ip == overlay_ip && p.peer_id != peer_id {
                    return Err(RegistryError::OverlayIpCollision(overlay_ip));
                }
            }
        }

        let network = nets.entry(net.clone()).or_insert_with(|| {
            let session = self.session_for_new_network();
            // Новая сеть → открыть relay-сессию (relay пересылает каждому кроме
            // отправителя, broadcast-модель TAP-overlay).
            self.relay.open(session);
            log::info!("network {} created (relay session {session})", net.as_str());
            Network {
                id: net.clone(),
                relay_session: session,
                peers: HashMap::new(),
            }
        });

        // Переподключение с тем же peer-id: обновить endpoint/srflx/nat, ссылку
        // на новый writer, сбросить presence. Рассылаем `PeerUpdated` остальным,
        // не дубль-запись. Новому контроль-соединению нужен `Welcome` тоже.
        if network.peers.contains_key(&peer_id) {
            // Scope: обновляем запись и вынимаем snapshot для broadcast, чтобы
            // закрыть mutable borrow до immutable-итерации по `network.peers`.
            let updated = {
                let existing = network
                    .peers
                    .get_mut(&peer_id)
                    .expect("checked contains_key above; invariant holds under write-lock");
                existing.overlay_ip = overlay_ip;
                existing.srflx = srflx;
                existing.nat = nat;
                existing.local_addr = local_addr;
                existing.control_addr = control_addr;
                existing.tx = tx;
                existing.last_seen = Instant::now();
                existing.missed_heartbeats = 0;
                existing.status = PeerStatus::Online;
                // Link'и сбрасываем — пути могут измениться после смены сети/NAT;
                // все per-pair link'и снова `Unknown` до новых punch-отчётов.
                existing.links.clear();
                existing.to_info(LinkKind::Unknown)
            };
            let session = network.relay_session;
            // Borrow закрыт — собираем список и рассылаем.
            let peers_list: Vec<PeerInfo> = network
                .peers
                .values()
                .filter(|p| p.peer_id != peer_id)
                .map(|p| p.to_info(LinkKind::Unknown))
                .collect();
            let msg = MeshServerMessage::PeerUpdated(updated);
            network.broadcast(&msg, Some(&peer_id));
            log::info!("peer {} re-joined network {} (updated)", peer_id.as_str(), net.as_str());
            return Ok(Welcome {
                peers: peers_list,
                relay_addr: self.relay_advertise.clone(),
                session,
            });
        }

        // Свежий пир: вставляем, рассылаем PeerJoined остальным, Welcome ему.
        let entry = PeerEntry {
            peer_id: peer_id.clone(),
            overlay_ip: overlay_ip.clone(),
            srflx: srflx.clone(),
            nat,
            local_addr,
            control_addr,
            last_seen: Instant::now(),
            missed_heartbeats: 0,
            status: PeerStatus::Online,
            links: HashMap::new(),
            tx,
        };
        let joined_info = entry.to_info(LinkKind::Unknown);
        let peers_list: Vec<PeerInfo> = network
            .peers
            .values()
            .map(|p| p.to_info(link_between(&entry, p)))
            .collect();
        network.peers.insert(peer_id.clone(), entry);
        let session = network.relay_session;
        let msg = MeshServerMessage::PeerJoined(joined_info);
        network.broadcast(&msg, Some(&peer_id));
        log::info!(
            "peer {} joined network {} (overlay {}, {} peers now)",
            peer_id.as_str(),
            net.as_str(),
            overlay_ip.as_str(),
            network.peers.len()
        );
        Ok(Welcome {
            peers: peers_list,
            relay_addr: self.relay_advertise.clone(),
            session,
        })
    }

    fn leave(&self, net: &NetworkId, peer_id: &PeerId) {
        let Ok(mut nets) = self.write() else { return };
        let Some(network) = nets.get_mut(net) else {
            return;
        };
        if network.peers.remove(peer_id).is_none() {
            return;
        }
        let msg = MeshServerMessage::PeerLeft { peer_id: peer_id.clone() };
        network.broadcast(&msg, None);
        let remaining = network.peers.len();
        let session = network.relay_session;
        if remaining == 0 {
            self.relay.close(session);
            nets.remove(net);
            log::info!("network {} emptied and removed (relay session {session} closed)", net.as_str());
        } else {
            log::info!("peer {} left network {} ({} remain)", peer_id.as_str(), net.as_str(), remaining);
        }
    }

    fn heartbeat(&self, net: &NetworkId, peer_id: &PeerId) {
        let Ok(mut nets) = self.write() else { return };
        let Some(network) = nets.get_mut(net) else {
            return;
        };
        let Some(p) = network.peers.get_mut(peer_id) else {
            return;
        };
        p.last_seen = Instant::now();
        p.missed_heartbeats = 0;
        if p.status != PeerStatus::Online {
            p.status = PeerStatus::Online;
            log::debug!("peer {} back online", peer_id.as_str());
        }
    }

    fn punch_report(&self, net: &NetworkId, from: &PeerId, to: &PeerId, kind: LinkKind) {
        let Ok(mut nets) = self.write() else { return };
        let Some(network) = nets.get_mut(net) else {
            return;
        };
        let Some(p) = network.peers.get_mut(from) else {
            return;
        };
        p.links.insert(to.clone(), kind);
        log::debug!("link {} -> {} = {kind:?}", from.as_str(), to.as_str());
    }

    fn kick(&self, net: &NetworkId, peer_id: &PeerId, reason: &str) {
        let Ok(mut nets) = self.write() else { return };
        let Some(network) = nets.get_mut(net) else {
            return;
        };
        let Some(p) = network.peers.remove(peer_id) else {
            return;
        };
        let kicked = MeshServerMessage::Kicked { reason: reason.to_string() };
        let _ = p.tx.send(kicked);
        let msg = MeshServerMessage::PeerLeft { peer_id: peer_id.clone() };
        network.broadcast(&msg, None);
        let session = network.relay_session;
        if network.peers.is_empty() {
            self.relay.close(session);
            nets.remove(net);
        }
        log::info!("peer {} kicked from network {} ({reason})", peer_id.as_str(), net.as_str());
    }

    fn close_network(&self, net: &NetworkId, reason: &str) {
        let Ok(mut nets) = self.write() else { return };
        let Some(network) = nets.remove(net) else {
            return;
        };
        let msg = MeshServerMessage::NetworkClosed { reason: reason.to_string() };
        network.broadcast(&msg, None);
        self.relay.close(network.relay_session);
        log::info!("network {} closed ({reason})", net.as_str());
    }

    fn snapshot(&self) -> Vec<NetworkSnapshot> {
        let Ok(nets) = self.inner.read() else {
            return Vec::new();
        };
        nets.values()
            .map(|n| NetworkSnapshot {
                network_id: n.id.clone(),
                relay_session: n.relay_session,
                peers: n
                    .peers
                    .values()
                    .map(|p| PeerSnapshot {
                        peer_id: p.peer_id.clone(),
                        overlay_ip: p.overlay_ip.clone(),
                        srflx: p.srflx.clone(),
                        nat: p.nat,
                        control_addr: p.control_addr.to_string(),
                        status: p.status,
                        links: p.links.iter().map(|(k, v)| (k.clone(), *v)).collect(),
                    })
                    .collect(),
            })
            .collect()
    }

    fn presence_sweep(
        &self,
        heartbeat_interval: Duration,
        offline_after: usize,
    ) -> Vec<(NetworkId, PeerId)> {
        let Ok(mut nets) = self.write() else { return Vec::new() };
        let now = Instant::now();
        // `usize → u32` через try_from: на 64-bit usize шире u32, `as u32`
        // обрезал бы (clippy::as_conversions). Порог мал, fallback к MAX безопасен.
        let degraded_mul = u32::try_from(HEARTBEAT_DEGRADED_AFTER).unwrap_or(u32::MAX);
        let offline_mul = u32::try_from(offline_after).unwrap_or(u32::MAX);
        let degraded_after = heartbeat_interval.saturating_mul(degraded_mul);
        let offline_after_dur = heartbeat_interval.saturating_mul(offline_mul);
        let mut removed = Vec::new();
        let mut empty_networks: Vec<NetworkId> = Vec::new();

        for net in nets.values_mut() {
            // Сначала обновим статусы не-протухшим, потом соберём оффлайн.
            let to_remove: Vec<PeerId> = {
                let mut dr = Vec::new();
                for p in net.peers.values_mut() {
                    let elapsed = now.saturating_duration_since(p.last_seen);
                    if elapsed > offline_after_dur {
                        p.status = PeerStatus::Offline;
                        dr.push(p.peer_id.clone());
                    } else if elapsed > degraded_after && p.status == PeerStatus::Online {
                        p.status = PeerStatus::Degraded;
                        log::debug!(
                            "peer {} degraded (no heartbeat for {:?})",
                            p.peer_id.as_str(),
                            elapsed
                        );
                    }
                }
                dr
            };
            for pid in &to_remove {
                if let Some(p) = net.peers.remove(pid) {
                    log::info!(
                        "peer {} offline in network {} (no heartbeat for {:?}); removing",
                        pid.as_str(),
                        net.id.as_str(),
                        now.saturating_duration_since(p.last_seen)
                    );
                    removed.push((net.id.clone(), pid.clone()));
                }
            }
            // Рассылаем PeerLeft всем оставшимся (одним проходом, под локом).
            for pid in &to_remove {
                let msg = MeshServerMessage::PeerLeft { peer_id: pid.clone() };
                net.broadcast(&msg, None);
            }
            if net.peers.is_empty() {
                self.relay.close(net.relay_session);
                empty_networks.push(net.id.clone());
            }
        }
        for nid in &empty_networks {
            nets.remove(nid);
            log::info!("network {} emptied (presence), removed", nid.as_str());
        }
        removed
    }
}

/// Текущий link между двумя пирами: смотрим в `links` первого, fallback
/// `Unknown` (punch ещё не отчёныван).
fn link_between(a: &PeerEntry, b: &PeerEntry) -> LinkKind {
    a.links.get(&b.peer_id).copied().unwrap_or(LinkKind::Unknown)
}

/// Blanket impl: `&R` — тоже `Registry`. Все методы `&self`, так что делегирование
/// тривиально. Нужен, чтобы `web::route(req, &registry)` работал без owned `R`
/// (caller передаёт ссылку на registry из потока-обработчика).
impl<R: Registry + ?Sized> Registry for &R {
    fn join(&self, req: JoinRequest) -> Result<Welcome, RegistryError> {
        (**self).join(req)
    }
    fn leave(&self, net: &NetworkId, peer_id: &PeerId) {
        (**self).leave(net, peer_id);
    }
    fn heartbeat(&self, net: &NetworkId, peer_id: &PeerId) {
        (**self).heartbeat(net, peer_id);
    }
    fn punch_report(&self, net: &NetworkId, from: &PeerId, to: &PeerId, kind: LinkKind) {
        (**self).punch_report(net, from, to, kind);
    }
    fn kick(&self, net: &NetworkId, peer_id: &PeerId, reason: &str) {
        (**self).kick(net, peer_id, reason);
    }
    fn close_network(&self, net: &NetworkId, reason: &str) {
        (**self).close_network(net, reason);
    }
    fn snapshot(&self) -> Vec<NetworkSnapshot> {
        (**self).snapshot()
    }
    fn presence_sweep(
        &self,
        heartbeat_interval: Duration,
        offline_after: usize,
    ) -> Vec<(NetworkId, PeerId)> {
        (**self).presence_sweep(heartbeat_interval, offline_after)
    }
}
