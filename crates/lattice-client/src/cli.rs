//! Разбор CLI и валидация в готовый `Setup`. Три взаимоисключающие ветки:
//! статический mesh Фазы 1 (`--peer`), динамический NAT-traversal Фазы 2
//! (`--rendezvous` + `--room`) и mesh coordination Фазы 3 (`--rendezvous` +
//! `--mesh`; `network-id` вычисляется из ключа, `--room` не нужен). Фазы 1-2
//! не сломаны — это отдельные режимы.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

use clap::Parser;

use lattice_client::crypto::{Crypto, Key, KEY_LEN};
use lattice_client::dynamic::DynamicParams;
use lattice_client::mesh::MeshParams;
use lattice_client::network_id;
use lattice_client::punch::{PunchConfig, DEFAULT_KEEPALIVE};
use lattice_client::transport::obfs::PaddingPolicy;
use lattice_client::transport::quic_tls::DEFAULT_SNI;
use lattice_client::transport::selector::TransportPreference;
use lattice_proto::{OverlayIp, PeerId, RoomId};

/// Публичные STUN-серверы по умолчанию. ДВА РАЗНЫХ оператора (разные IP) —
/// эвристике symmetric vs cone нужны именно разные таргеты, чтобы сравнить
/// внешний порт маппинга. См. `stun::discover`.
const DEFAULT_STUN: [&str; 2] = ["stun.l.google.com:19302", "stun.cloudflare.com:3478"];

#[derive(Parser, Debug)]
#[command(
    name = "lattice-client",
    version,
    about = "Lattice client — LAN overlay over UDP + ChaCha20-Poly1305 (static mesh or NAT traversal)"
)]
pub struct Cli {
    /// TAP IP в CIDR-нотации, напр. 10.66.0.1/24.
    #[arg(long, value_name = "IP/PREFIX")]
    tap_ip: String,
    /// Локальный UDP-адрес бинда. В динамическом режиме можно `0.0.0.0:0`.
    #[arg(long, value_name = "ADDR")]
    listen: SocketAddr,
    /// 32-байтный pre-shared ключ в hex (64 символа).
    #[arg(long, value_name = "HEX64")]
    key: String,

    /// [static] Адрес пира (повторяемый) — прямой mesh Фазы 1.
    #[arg(long, value_name = "ADDR")]
    peer: Vec<SocketAddr>,

    /// [dynamic] Адрес rendezvous-сервера `host:port` (control-канал).
    #[arg(long, value_name = "ADDR")]
    rendezvous: Option<String>,
    /// [dynamic] Идентификатор комнаты — по нему сервер сводит двух пиров.
    #[arg(long, value_name = "ID")]
    room: Option<String>,
    /// [mesh] Включить mesh-режим Фазы 3 (coordination-сервер, N пиров на сеть).
    /// `network-id` вычисляется из `--key` (BLAKE3), `--room` не нужен.
    #[arg(long, default_value_t = false)]
    mesh: bool,
    /// [mesh] Идентификатор пира (иначе генерируется `hostname-pid`).
    #[arg(long, value_name = "ID")]
    peer_id: Option<String>,
    /// [dynamic/mesh] STUN-сервер (повторяемый). По умолчанию — два публичных.
    #[arg(long, value_name = "ADDR")]
    stun: Vec<String>,
    /// [dynamic] Интервал keepalive в секундах (держит NAT-биндинг живым).
    #[arg(long, value_name = "SECS", default_value_t = DEFAULT_KEEPALIVE.as_secs())]
    keepalive_secs: u64,
    /// [dynamic] Сколько секунд ждать второго пира в комнате.
    #[arg(long, value_name = "SECS", default_value_t = 120)]
    match_timeout_secs: u64,

    /// [Фаза 4] Транспорт датаплейна: `auto` (UDP, при неуспехе эскалация на
    /// QUIC), `udp` (только голый UDP, как Фазы 1-3), `quic` (сразу QUIC/h3).
    #[arg(long, value_name = "auto|udp|quic", default_value = "auto")]
    transport: String,
    /// [Фаза 4] SNI для QUIC-handshake (мимикрия под обычный CDN-хост). НЕ
    /// domain-fronting — просто правдоподобное имя в `ClientHello`.
    #[arg(long, value_name = "HOST")]
    sni: Option<String>,
    /// [Фаза 4] Включить padding длин (добивать мелкие пакеты до типичного
    /// размера). Цена — рост трафика; cap на оверхед внутри. ОБА пира должны
    /// включить (меняет wire-формат обёртки).
    #[arg(long, default_value_t = false)]
    obfs_padding: bool,
    /// [Фаза 4] Включить timing jitter на служебных пакетах (keepalive/heartbeat)
    /// — ломать машинно-регулярный ритм. Цена — небольшой разброс задержки
    /// служебных пакетов (датаплейн не трогается).
    #[arg(long, default_value_t = false)]
    obfs_jitter: bool,
}

/// Готовая к запуску конфигурация.
pub struct Setup {
    pub ip: Ipv4Addr,
    pub prefix_len: u8,
    pub crypto: Crypto,
    pub mode: Mode,
    pub transport: TransportConfig,
}

/// Конфигурация транспорта/обфускации Фазы 4. Применяется к датаплейну
/// независимо от режима (static/dynamic/mesh).
pub struct TransportConfig {
    pub preference: TransportPreference,
    /// SNI для QUIC-handshake.
    pub sni: String,
    /// Политика padding. `None` — обёртка НЕ применяется (wire-формат как в
    /// Фазах 1-3, совместимость с непропатченными пирами). `Some` — оба пира
    /// обязаны включить (obfs-обёртка меняет формат).
    pub padding: Option<PaddingPolicy>,
    /// jitter на служебных пакетах (keepalive/heartbeat).
    pub jitter: bool,
}

impl TransportConfig {
    /// Эффективный MTU TAP с учётом транспорта. При forced-QUIC вычитаем оверхед
    /// QUIC+DATAGRAM (иначе крупные фреймы тихо дропались бы). При `auto`/`udp`
    /// держим базовый: auto стартует на UDP, а пересчёт MTU при эскалации на
    /// QUIC — часть установления (re-set MTU), не статическая величина.
    #[must_use]
    pub fn effective_mtu(&self, base_mtu: u32) -> u32 {
        if matches!(self.preference, TransportPreference::Quic) {
            let base = usize::try_from(base_mtu).unwrap_or(usize::MAX);
            let reduced = lattice_client::transport::quic_effective_mtu(base);
            u32::try_from(reduced).unwrap_or(base_mtu)
        } else {
            base_mtu
        }
    }
}

/// Режим работы датаплейна.
pub enum Mode {
    Static(StaticParams),
    Dynamic(Box<DynamicRun>),
    Mesh(Box<MeshRun>),
}

pub struct StaticParams {
    pub listen: SocketAddr,
    pub peers: Vec<SocketAddr>,
}

pub struct DynamicRun {
    pub listen: SocketAddr,
    pub params: DynamicParams,
    pub keepalive: Duration,
}

/// Параметры mesh-режима Фазы 3.
pub struct MeshRun {
    pub listen: SocketAddr,
    pub params: MeshParams,
    pub heartbeat: Duration,
}

impl Cli {
    /// Провалидировать аргументы и собрать `Setup`. Возвращает человекочитаемую
    /// ошибку (без паники) на любой неверный ввод.
    ///
    /// # Errors
    ///
    /// Текстовая ошибка: битый `--key`/`--tap-ip`, отсутствие режима, или
    /// `--rendezvous` без `--room`.
    pub fn validate(self) -> Result<Setup, String> {
        let key = parse_key(&self.key).map_err(|m| format!("invalid --key: {m}"))?;
        let (ip, prefix_len) = parse_cidr(&self.tap_ip).map_err(|m| format!("invalid --tap-ip: {m}"))?;
        let crypto = Crypto::new(&key);

        let transport = TransportConfig {
            preference: TransportPreference::parse(&self.transport)
                .map_err(|m| format!("invalid --transport: {m}"))?,
            sni: self.sni.clone().unwrap_or_else(|| DEFAULT_SNI.to_string()),
            padding: if self.obfs_padding {
                Some(PaddingPolicy::default())
            } else {
                None
            },
            jitter: self.obfs_jitter,
        };

        let mode = match (&self.rendezvous, self.mesh, self.peer.is_empty()) {
            (Some(rendezvous), true, _) => {
                // Mesh-режим Фазы 3: network-id из ключа, peer-id из CLI или
                // генерируется `hostname-pid`.
                let network_id = network_id::from_key(&key)
                    .map_err(|e| format!("cannot compute network-id: {e}"))?;
                let peer_id = PeerId::new(self.peer_id.clone().unwrap_or_else(generate_peer_id));
                let overlay_ip = OverlayIp::new(ip.to_string());
                let stun_servers = resolve_stun(&pick_stun(&self.stun));
                let params = MeshParams {
                    rendezvous: rendezvous.clone(),
                    network_id,
                    peer_id,
                    overlay_ip,
                    stun_servers,
                    connect_timeout: Duration::from_secs(10),
                    hello_timeout: Duration::from_secs(15),
                    stun_timeout: Duration::from_secs(3),
                    punch: PunchConfig::default(),
                    heartbeat_interval: Duration::from_secs(15),
                };
                Mode::Mesh(Box::new(MeshRun {
                    listen: self.listen,
                    params,
                    heartbeat: Duration::from_secs(15),
                }))
            }
            (Some(rendezvous), false, _) => {
                let room = self
                    .room
                    .clone()
                    .ok_or("--rendezvous requires --room (or --mesh)")?;
                let stun_servers = resolve_stun(&pick_stun(&self.stun));
                let params = DynamicParams {
                    rendezvous: rendezvous.clone(),
                    room: RoomId::new(room),
                    stun_servers,
                    connect_timeout: Duration::from_secs(10),
                    match_timeout: Duration::from_secs(self.match_timeout_secs),
                    stun_timeout: Duration::from_secs(3),
                    punch: PunchConfig::default(),
                };
                Mode::Dynamic(Box::new(DynamicRun {
                    listen: self.listen,
                    params,
                    keepalive: Duration::from_secs(self.keepalive_secs),
                }))
            }
            (None, _, false) => Mode::Static(StaticParams {
                listen: self.listen,
                peers: dedup(self.peer),
            }),
            (None, _, true) => {
                return Err(
                    "no mode selected: pass --peer (static), --rendezvous --room (NAT traversal), or --rendezvous --mesh (coordination)"
                        .into(),
                )
            }
        };

        Ok(Setup {
            ip,
            prefix_len,
            crypto,
            mode,
            transport,
        })
    }
}

fn pick_stun(user: &[String]) -> Vec<String> {
    if user.is_empty() {
        DEFAULT_STUN.iter().map(|s| (*s).to_string()).collect()
    } else {
        user.to_vec()
    }
}

/// Сгенерировать `peer-id` как `hostname-pid`. Hostname из `HOSTNAME` env (или
/// "peer" если не задано), pid — `std::process::id()`. Стабильно в рамках
/// процесса, уникально между машинами. Сервер использует как ключ в реестре.
fn generate_peer_id() -> String {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "peer".to_string());
    let host = host.split('.').next().unwrap_or("peer");
    format!("{host}-{}", std::process::id())
}

/// Разрешить STUN-хосты в `SocketAddr`. Нерезолвящиеся — пропускаем с warning'ом
/// (деградация, не паника): меньше таргетов = хуже эвристика, но не отказ.
fn resolve_stun(specs: &[String]) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for spec in specs {
        match spec.to_socket_addrs() {
            Ok(mut addrs) => {
                if let Some(a) = addrs.find(SocketAddr::is_ipv4) {
                    log::info!("STUN target {spec} -> {a}");
                    out.push(a);
                } else {
                    log::warn!("STUN target {spec} resolved to no IPv4 address, skipped");
                }
            }
            Err(e) => log::warn!("cannot resolve STUN target {spec}: {e}"),
        }
    }
    out
}

fn dedup(mut v: Vec<SocketAddr>) -> Vec<SocketAddr> {
    v.sort();
    v.dedup();
    v
}

/// Распарсить hex-ключ: ровно 64 hex-символа → 32 байта.
fn parse_key(s: &str) -> Result<Key, String> {
    let bytes = hex::decode(s.trim()).map_err(|e| e.to_string())?;
    let arr: [u8; KEY_LEN] = bytes
        .try_into()
        .map_err(|v: Vec<u8>| format!("expected {KEY_LEN} bytes (64 hex chars), got {}", v.len()))?;
    Ok(Key::new(arr))
}

/// Распарсить «IP/PREFIX» → (`Ipv4Addr`, u8). Поддерживается только IPv4.
fn parse_cidr(s: &str) -> Result<(Ipv4Addr, u8), String> {
    let (ip_part, prefix_part) = s
        .split_once('/')
        .ok_or_else(|| format!("expected IP/PREFIX, got '{s}'"))?;
    let ip: IpAddr = ip_part.parse().map_err(|e| format!("invalid IP '{ip_part}': {e}"))?;
    let ip = match ip {
        IpAddr::V4(v4) => v4,
        IpAddr::V6(_) => return Err("IPv6 not supported".into()),
    };
    let prefix: u8 = prefix_part
        .parse()
        .map_err(|e| format!("invalid prefix '{prefix_part}': {e}"))?;
    Ok((ip, prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_hex_roundtrip() {
        let k = parse_key(&"00".repeat(KEY_LEN)).expect("valid hex");
        assert_eq!(k.as_bytes(), &[0u8; KEY_LEN]);
    }

    #[test]
    fn key_wrong_length_rejected() {
        assert!(parse_key("0011").is_err());
        assert!(parse_key(&"ab".repeat(KEY_LEN - 1)).is_err());
    }

    #[test]
    fn key_non_hex_rejected() {
        assert!(parse_key(&"zz".repeat(KEY_LEN * 2)).is_err());
    }

    #[test]
    fn cidr_parses() {
        let (ip, p) = parse_cidr("10.66.0.1/24").expect("cidr");
        assert_eq!(ip, Ipv4Addr::new(10, 66, 0, 1));
        assert_eq!(p, 24);
    }

    #[test]
    fn cidr_missing_prefix() {
        assert!(parse_cidr("10.66.0.1").is_err());
    }

    #[test]
    fn cidr_ipv6_rejected() {
        assert!(parse_cidr("::1/128").is_err());
    }

    #[test]
    fn pick_stun_defaults_when_empty() {
        assert_eq!(pick_stun(&[]).len(), DEFAULT_STUN.len());
        assert_eq!(pick_stun(&["x:1".to_string()]), vec!["x:1".to_string()]);
    }

    fn cfg(pref: TransportPreference) -> TransportConfig {
        TransportConfig {
            preference: pref,
            sni: DEFAULT_SNI.to_string(),
            padding: None,
            jitter: false,
        }
    }

    #[test]
    fn effective_mtu_reduced_only_for_quic() {
        // udp/auto держат базовый MTU; forced-quic опускает на QUIC-оверхед.
        assert_eq!(cfg(TransportPreference::Udp).effective_mtu(1380), 1380);
        assert_eq!(cfg(TransportPreference::Auto).effective_mtu(1380), 1380);
        assert!(cfg(TransportPreference::Quic).effective_mtu(1380) < 1380);
    }
}
