//! Выбор транспорта для Фазы 4 — явная машина состояний, не каскад `if`.
//!
//! ## Зачем эскалация именно так
//!
//! Голый UDP — самый дешёвый путь: ноль QUIC/TLS-оверхеда, минимальная задержка,
//! прямой p2p после punch. Поэтому в `auto` пробуем его ПЕРВЫМ. Если прямой UDP
//! не встал (punch не сошёлся целиком, или поток подозрительно режется), это
//! сигнал, что «непонятный шифрованный UDP» может фильтроваться — эскалируем на
//! QUIC, который мимикрирует под HTTP/3 (ALPN h3, TLS-handshake). QUIC дороже
//! (handshake + двойная инкапсуляция, меньше эффективный MTU), поэтому он —
//! fallback, а не дефолт пути.
//!
//! ## Честная планка
//!
//! Это машина ВЫБОРА транспорта, а не детектор DPI. Мы не знаем наверняка,
//! «зарезали» нас или просто NAT не пробился — эвристика грубая (не встал =>
//! пробуем замаскированный путь). QUIC даёт «не матчится по сигнатуре известных
//! VPN и не выделяется пассивной эвристикой как непонятный UDP»; против активного
//! пробинга не тестировалось (см. AGENTS.md «Честная планка Фазы 4»).
//!
//! ## Защита от зацикливания
//!
//! `auto` мог бы крутить udp→quic→udp→… вечно. Поэтому: один «цикл» = одна
//! попытка UDP + одна попытка QUIC; между циклами — backoff; число циклов
//! ограничено `max_cycles`. Исчерпали → `Exhausted`, наружу идёт явный отказ,
//! не бесконечный ретрай.

use std::time::Duration;

/// Какой транспорт использовать. Newtype-enum вместо строк/чисел — нельзя
/// случайно передать «udp» как опечатку.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    /// Голый UDP (Фазы 1-3). Самый дешёвый, прямой p2p.
    Udp,
    /// QUIC поверх UDP (Фаза 4). Маскировка под HTTP/3, дороже.
    Quic,
}

impl TransportKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TransportKind::Udp => "udp",
            TransportKind::Quic => "quic",
        }
    }
}

/// Что выбрал оператор (`--transport`). `Auto` — машина решает с эскалацией.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportPreference {
    /// Пробовать UDP, при неуспехе эскалировать на QUIC (дефолт).
    Auto,
    /// Только голый UDP (как Фазы 1-3) — никакой маскировки.
    Udp,
    /// Сразу QUIC — оператор уже знает, что прямой UDP режется.
    Quic,
}

impl TransportPreference {
    /// Разобрать значение CLI. `Err` с подсказкой на неизвестном.
    ///
    /// # Errors
    ///
    /// Текстовая ошибка, если строка не `auto`/`udp`/`quic`.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(TransportPreference::Auto),
            "udp" => Ok(TransportPreference::Udp),
            "quic" => Ok(TransportPreference::Quic),
            other => Err(format!("unknown transport '{other}' (expected auto|udp|quic)")),
        }
    }
}

/// Параметры эскалации. Newtype вокруг голых чисел, дефолты — в `Default`.
#[derive(Debug, Clone, Copy)]
pub struct EscalationPolicy {
    /// Сколько полных циклов (UDP+QUIC) пробовать в `auto`, прежде чем сдаться.
    pub max_cycles: u32,
    /// Пауза между циклами — не молотить сервер/сеть вплотную.
    pub backoff: Duration,
}

impl Default for EscalationPolicy {
    fn default() -> Self {
        // 3 цикла = до 6 попыток установления; backoff 2с — заметно, но не
        // томит пользователя. Подобрано как разумный потолок для PoC/MVP.
        Self {
            max_cycles: 3,
            backoff: Duration::from_secs(2),
        }
    }
}

/// Внутреннее состояние машины. Переходы прокомментированы «почему».
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Фиксированный транспорт (`--transport udp|quic`, либо `auto` после
    /// успеха): возвращаем его, на неуспехе — backoff-ретрай тем же до cap.
    Settled(TransportKind),
    /// `auto`: следующая попытка — UDP (самый дешёвый, пробуем первым).
    TryUdp,
    /// `auto`: UDP не встал → эскалация на QUIC (HTTP/3-мимикрия).
    TryQuic,
    /// Потолок попыток исчерпан — наружу явный отказ, не вечный ретрай.
    Exhausted,
}

/// Машина выбора транспорта с auto-эскалацией, backoff и потолком попыток.
/// Caller дёргает `next` (что пробовать), затем `record_success`/`record_failure`.
pub struct TransportSelector {
    state: State,
    policy: EscalationPolicy,
    /// Сколько полных циклов уже отработано (для cap в `auto`).
    cycles: u32,
    /// Для фиксированного режима — счётчик неудач, чтобы тоже не ретраить вечно.
    fixed_failures: u32,
}

impl TransportSelector {
    #[must_use]
    pub fn new(pref: TransportPreference, policy: EscalationPolicy) -> Self {
        let state = match pref {
            TransportPreference::Auto => State::TryUdp,
            TransportPreference::Udp => State::Settled(TransportKind::Udp),
            TransportPreference::Quic => State::Settled(TransportKind::Quic),
        };
        Self {
            state,
            policy,
            cycles: 0,
            fixed_failures: 0,
        }
    }

    /// Какой транспорт пробовать сейчас. `None` — попытки исчерпаны (caller
    /// сообщает отказ, не зацикливается).
    #[must_use]
    pub fn next(&self) -> Option<TransportKind> {
        match self.state {
            State::Settled(kind) => Some(kind),
            State::TryUdp => Some(TransportKind::Udp),
            State::TryQuic => Some(TransportKind::Quic),
            State::Exhausted => None,
        }
    }

    /// Транспорт встал — фиксируемся на нём (повторные `next` дают тот же).
    pub fn record_success(&mut self, kind: TransportKind) {
        self.state = State::Settled(kind);
        self.fixed_failures = 0;
    }

    /// Попытка не удалась. Возвращает рекомендованную паузу перед следующей
    /// (backoff на границе цикла), либо `None`, если пора сдаться. Переходы:
    /// `TryUdp` → `TryQuic` (эскалация, без паузы — пробуем замаскированный
    /// путь сразу); `TryQuic` → (cycle++, если < cap) `TryUdp` после backoff,
    /// иначе `Exhausted`; `Settled` → backoff-ретрай тем же до cap.
    pub fn record_failure(&mut self) -> Option<Duration> {
        match self.state {
            State::TryUdp => {
                // UDP не встал → сразу эскалируем на QUIC, без паузы: маскировка
                // может пройти там, где голый UDP зарезан.
                self.state = State::TryQuic;
                Some(Duration::ZERO)
            }
            State::TryQuic => {
                self.cycles += 1;
                if self.cycles >= self.policy.max_cycles {
                    self.state = State::Exhausted;
                    None
                } else {
                    // Новый цикл с UDP после backoff — вдруг это была разовая
                    // потеря, а не постоянная фильтрация.
                    self.state = State::TryUdp;
                    Some(self.policy.backoff)
                }
            }
            State::Settled(_) => {
                self.fixed_failures += 1;
                if self.fixed_failures >= self.policy.max_cycles {
                    self.state = State::Exhausted;
                    None
                } else {
                    Some(self.policy.backoff)
                }
            }
            State::Exhausted => None,
        }
    }

    /// Исчерпаны ли попытки (для явного сообщения наружу).
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        matches!(self.state, State::Exhausted)
    }
}

/// Кто из пары слушает QUIC (а кто дозванивается). QUIC connection-oriented:
/// одна сторона — listener, другая — client; если ОБЕ слушают или ОБЕ звонят,
/// соединение не встанет. Решаем детерминированно по `peer-id`, которые ОБА
/// пира уже знают из mesh (`Welcome`/`PeerInfo`): меньший id слушает. Это не
/// требует ни нового signaling-сообщения, ни изменений сервера — обе стороны
/// независимо приходят к одному распределению ролей. Tie (равные id) не бывает:
/// peer-id уникальны в сети (ключ реестра).
#[must_use]
pub fn quic_listener_first(self_id: &str, peer_id: &str) -> bool {
    self_id < peer_id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preference_parses() {
        assert_eq!(TransportPreference::parse("AUTO").unwrap(), TransportPreference::Auto);
        assert_eq!(TransportPreference::parse(" udp ").unwrap(), TransportPreference::Udp);
        assert_eq!(TransportPreference::parse("quic").unwrap(), TransportPreference::Quic);
        assert!(TransportPreference::parse("wireguard").is_err());
    }

    #[test]
    fn auto_escalates_udp_then_quic() {
        let mut s = TransportSelector::new(TransportPreference::Auto, EscalationPolicy::default());
        assert_eq!(s.next(), Some(TransportKind::Udp));
        // UDP fail → QUIC сразу (нулевая пауза).
        assert_eq!(s.record_failure(), Some(Duration::ZERO));
        assert_eq!(s.next(), Some(TransportKind::Quic));
    }

    #[test]
    fn auto_settles_on_success() {
        let mut s = TransportSelector::new(TransportPreference::Auto, EscalationPolicy::default());
        s.record_failure(); // udp fail → quic
        s.record_success(TransportKind::Quic);
        assert_eq!(s.next(), Some(TransportKind::Quic));
        assert_eq!(s.next(), Some(TransportKind::Quic)); // фиксирован
    }

    #[test]
    fn auto_exhausts_after_cap_without_looping() {
        let policy = EscalationPolicy {
            max_cycles: 2,
            backoff: Duration::from_millis(1),
        };
        let mut s = TransportSelector::new(TransportPreference::Auto, policy);
        // Цикл 1: udp fail → quic fail.
        assert_eq!(s.next(), Some(TransportKind::Udp));
        s.record_failure();
        assert_eq!(s.next(), Some(TransportKind::Quic));
        assert_eq!(s.record_failure(), Some(Duration::from_millis(1))); // новый цикл
        // Цикл 2: udp fail → quic fail → exhausted.
        assert_eq!(s.next(), Some(TransportKind::Udp));
        s.record_failure();
        assert_eq!(s.next(), Some(TransportKind::Quic));
        assert_eq!(s.record_failure(), None); // cap
        assert!(s.is_exhausted());
        assert_eq!(s.next(), None);
    }

    #[test]
    fn fixed_udp_never_escalates_to_quic() {
        let mut s = TransportSelector::new(TransportPreference::Udp, EscalationPolicy::default());
        assert_eq!(s.next(), Some(TransportKind::Udp));
        s.record_failure();
        // Остаётся UDP (фиксированный режим не эскалирует), пока не исчерпает cap.
        assert_eq!(s.next(), Some(TransportKind::Udp));
    }

    #[test]
    fn quic_roles_are_complementary() {
        // Обе стороны независимо приходят к согласованным ролям: ровно одна
        // слушает. Без этого QUIC не встал бы (обе звонят / обе слушают).
        assert!(quic_listener_first("alice", "bob"));
        assert!(!quic_listener_first("bob", "alice"));
        // Симметрия: ровно один из пары — listener.
        assert_ne!(
            quic_listener_first("peer-1", "peer-2"),
            quic_listener_first("peer-2", "peer-1")
        );
    }

    #[test]
    fn fixed_exhausts_after_cap() {
        let policy = EscalationPolicy {
            max_cycles: 2,
            backoff: Duration::from_millis(1),
        };
        let mut s = TransportSelector::new(TransportPreference::Quic, policy);
        assert_eq!(s.record_failure(), Some(Duration::from_millis(1)));
        assert_eq!(s.record_failure(), None);
        assert!(s.is_exhausted());
    }
}
