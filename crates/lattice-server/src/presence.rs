//! Presence-поток: периодически вызывает `registry.presence_sweep`, который
//! помечает `Degraded`/`Offline` и удаляет протухших пиров. Порог — 3 пропуска
//! heartbeat (~45с при 15с-интервале), не одна потеря: разовые UDP/TCP-лаг не
//! выкидывают пира. Сама логика — в реестре (он знает структуру и держит lock);
//! здесь только ритм и логирование удалённых.
//!
//! Поток никогда не возвращается штатно — крутится, пока процесс жив. Ошибки
//! одной итерации логируются и не роняют цикл (presence — фоновая servicio).

use std::thread;
use std::time::Duration;

use crate::registry::{Registry, HEARTBEAT_OFFLINE_AFTER};

/// Интервал heartbeat по умолчанию (клиент шлёт каждые 15с). Ниже типичного
/// NAT-таймаута, но presence-порог — 3 пропуска = ~45с, что больше лаг-окна.
pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// Запустить presence-поток. Блокирующий; для main — `thread::spawn`.
pub fn serve<R: Registry + 'static>(registry: &R, heartbeat_interval: Duration) {
    loop {
        thread::sleep(heartbeat_interval);
        let removed = registry.presence_sweep(heartbeat_interval, HEARTBEAT_OFFLINE_AFTER);
        if !removed.is_empty() {
            log::info!(
                "presence sweep: removed {} stale peer(s) across networks",
                removed.len()
            );
            for (net, peer) in &removed {
                log::debug!("presence removed peer {} from network {}", peer.as_str(), net.as_str());
            }
        }
    }
}
