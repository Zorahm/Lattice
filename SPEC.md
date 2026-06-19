# Lattice — overlay-сеть для локалки (Windows-клиент + Linux-сервер, PoC → MVP)

## Что строим

Лёгкая программа, создающая виртуальную локальную сеть поверх интернета — аналог Hamachi/RadminVPN/ZeroTier, но **без WireGuard** (его сигнатура палится российским DPI). Цель — не VPN-выход в интернет, а **mesh-overlay**: несколько машин видят друг друга как в одной LAN, включая broadcast/LAN-discovery для игр (Minecraft LAN, SA:MP, Project Zomboid).

Ключевое архитектурное решение: **L2 (TAP)**, а не L3 (TUN). L2 даёт настоящий Ethernet-broadcast/multicast → discovery работает из коробки.

## Разделение client / server

Два разных артефакта в одном Cargo workspace:

- **`lattice-client`** — desktop-программа, **только Windows**. Поднимает виртуальный L2-адаптер (tap-windows6), шифрует/гоняет Ethernet-фреймы по UDP. Требует TAP-драйвер и права администратора.
- **`lattice-server`** — coordination/relay сервер (появляется на Фазе 3), **кроссплатформенный, целевой деплой — Linux VPS**. TAP/Win32 не трогает вообще: чистый сетевой сервис (signaling, реестр пиров, relay). Никаких платформенных зависимостей клиента.
- **`lattice-proto`** — shared-крейт: общие типы протокола между client и server (сообщения регистрации, описание пира, endpoint, форматы сериализации через serde). Чтобы контракт не разъезжался.

На Фазе 1 сервера ещё нет — только `lattice-client` и `lattice-proto` (даже если proto пока почти пустой). Workspace-структуру закладываем сразу, чтобы Фаза 3 не ломала layout.

```
lattice/
├── Cargo.toml            # workspace
├── crates/
│   ├── lattice-client/   # Windows-only, TAP + crypto + transport
│   ├── lattice-server/   # Linux/любой, появляется на Фазе 3
│   └── lattice-proto/    # shared типы протокола
```

## Почему tap-windows6, а не Wintun (важно, клиент)

На Windows два драйвера виртуального адаптера:

- **Wintun** (от WireGuard) — быстрый, но **L3 (TUN)**: только IP-пакеты, нет Ethernet-фреймов, ARP, broadcast. Discovery игр не заработает без ручной эмуляции.
- **tap-windows6** (от OpenVPN) — **L2 (TAP)**: полноценный виртуальный Ethernet-адаптер, есть ARP/broadcast/multicast.

LAN-discovery — главная причина брать L2, поэтому **берём tap-windows6**.

## Стек

**Клиент (lattice-client):**
- Rust (edition 2021), target `x86_64-pc-windows-msvc`.
- Виртуальный интерфейс: tap-windows6 (L2). Win32 API через crate `windows`/`windows-sys`: открытие `\\.\Global\<GUID>.tap` через `CreateFileW` с `FILE_FLAG_OVERLAPPED`, конфиг через `DeviceIoControl` (`TAP_IOCTL_SET_MEDIA_STATUS` для поднятия линка). Без сырого `extern`.
- Транспорт: UDP-сокет. Кастомный протокол — нет известной VPN-сигнатуры, DPI не классифицирует как WG/OpenVPN.
- Шифрование: `chacha20poly1305` (AEAD). На фрейм — случайный 12-байтный nonce перед ciphertext.
- Ключ: PoC — общий pre-shared key (32 байта hex из аргумента/env). MVP — от сервера.
- `unsafe` только на FFI-границе с драйвером, изолировать в одном модуле. Файлы ≤300 строк. Без `unwrap()` в горячем пути.

**Сервер (lattice-server, с Фазы 3):**
- Rust, кроссплатформенный, деплой Linux. `tokio` + `axum` (WebSocket для signaling). Никаких TAP/Win32 зависимостей.

## Зависимости рантайма (Windows-клиент)

- **tap-windows6 driver** должен быть установлен (OpenVPN-инсталлятор или `tap-windows.exe`). Без драйвера клиент не стартует — зафиксировать в README.
- Создание TAP-адаптера и поднятие линка требуют **прав администратора**. Клиент внятно сообщает, если запущен без них.
- Назначение IP — через `netsh interface ip set address` либо IP Helper API (`CreateUnicastIpAddressEntry`). Для PoC допустим `netsh`.

## Формат UDP-датаграммы

```
[ nonce: 12 байт ][ ChaCha20-Poly1305(ethernet_frame): N+16 байт ]
```

Получатель: split nonce/ciphertext → decrypt → если AEAD ок, пишет plaintext Ethernet-фрейм в TAP. Невалидные/чужие пакеты молча дропаются (AEAD-тег сам отсекает).

## Архитектура клиента (модули lattice-client)

- `main.rs` — парсинг аргументов, запуск двух воркеров (tap→udp, udp→tap), graceful shutdown по Ctrl+C.
- `crypto.rs` — AEAD-обёртка: `seal(frame) -> datagram`, `open(datagram) -> Option<frame>`. Nonce через `OsRng`.
- `tap.rs` — **вся Windows-FFI здесь**: поиск/открытие tap-windows6, IOCTL поднятия линка, overlapped read/write фреймов. Единственное место с `unsafe`. Наружу — безопасный `TapDevice { read_frame, write_frame }`.
- `transport.rs` — UDP-сокет, отправка/приём. За trait `Transport`.
- `peers.rs` — реестр пиров (PoC: статический список `SocketAddr` из аргументов). За trait `Discovery`.
- `netcfg.rs` — назначение IP / MTU (netsh-обёртка или IP Helper API).

Два рабочих цикла:
```
tap_reader:  TAP.read() → crypto.seal() → udp.send_to(peer)
udp_reader:  udp.recv() → crypto.open() → TAP.write()
```

Из-за overlapped I/O TAP read/write асинхронны — для PoC проще два std-потока с blocking-обёрткой поверх overlapped (`GetOverlappedResult`).

## Фазы

**Фаза 1 — PoC (главная цель). Только lattice-client.**
Запуск (от администратора):
```
lattice-client.exe --tap-ip 10.66.0.1/24 --listen 0.0.0.0:51820 --peer <ip>:51820 --key <hex32>
lattice-client.exe --tap-ip 10.66.0.2/24 --listen 0.0.0.0:51820 --peer <ip>:51820 --key <hex32>
```
Критерий готовности: две Windows-машины (или VM) пингуют друг друга по 10.66.0.0/24; Wireshark на TAP-адаптере видит ARP-broadcast → discovery будет работать. Бинарь сам находит tap-windows6 адаптер, поднимает линк, назначает IP и MTU ~1380, корректно опускает линк при выходе.

**Фаза 2 — NAT traversal.** STUN (внешний endpoint), UDP hole punching, fallback на relay при symmetric NAT. За trait `Discovery`, Фаза 1 не переписывается.

**Фаза 3 — coordination-сервер. Появляется lattice-server (Linux).** axum + WebSocket: реестр пиров, раздача ключей/endpoint'ов, координация hole punching, relay. Клиент при старте регится, получает список пиров. Общие типы — в `lattice-proto`.

**Фаза 4 — обфускация транспорта (если DPI начнёт резать эвристикой).** Подмена голого UDP на QUIC (`quinn`, выглядит как HTTP/3) + паддинг/рандомизация таймингов, за trait `Transport`, без переписывания crypto/tap.

## Важные требования к архитектуре

- **Сменяемость транспортного слоя** — главный долгосрочный риск это гонка с DPI. Датаплейн за `trait Transport { fn send(&self, addr, &[u8]); fn recv(&self) -> (Vec<u8>, SocketAddr); }`. Discovery — за `trait Discovery`. Crypto и TAP от транспорта не зависят.
- **FFI-изоляция.** Весь `unsafe`/Win32 — только в `tap.rs`. Сервер Win32 не касается в принципе.
- **client/server не делят платформенные зависимости.** `lattice-server` не должен тянуть `windows` crate даже транзитивно. Платформенное — только в client. Общее — только в proto (чистый serde, no_std-friendly по возможности).
- **MTU:** после инкапсуляции (nonce+tag+UDP+IP) датаграмма больше 1500 → MTU TAP ~1380 против фрагментации.
- **Поиск адаптера:** tap-windows6 перечисляются в реестре (`SYSTEM\CurrentControlSet\Control\Class\{4d36e972-...}`) + connections по ComponentId `tap0901`. Найти GUID до открытия `\\.\Global\<GUID>.tap`.

## Чего НЕ делаем

- Не используем WireGuard / boringtun / wireguard-go / Wintun (Wintun = L3, broadcast не даёт).
- Не изобретаем криптопримитивы — готовый ChaCha20-Poly1305.
- Без GUI на PoC — только CLI.
- Не пишем сервер на Фазе 1 — но workspace и proto-крейт закладываем сразу.

## Подсказка по реализации crypto.rs

```rust
use chacha20poly1305::{aead::{Aead, KeyInit, OsRng}, ChaCha20Poly1305, Nonce, Key};
use rand::RngCore;

pub struct Crypto { cipher: ChaCha20Poly1305 }

impl Crypto {
    pub fn new(key: &[u8; 32]) -> Self {
        Self { cipher: ChaCha20Poly1305::new(Key::from_slice(key)) }
    }
    pub fn seal(&self, frame: &[u8]) -> Option<Vec<u8>> {
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let ct = self.cipher.encrypt(Nonce::from_slice(&nonce), frame).ok()?;
        let mut out = Vec::with_capacity(12 + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Some(out)
    }
    pub fn open(&self, datagram: &[u8]) -> Option<Vec<u8>> {
        if datagram.len() < 12 { return None; }
        let (nonce, ct) = datagram.split_at(12);
        // decrypt сам проверяет AEAD-тег → чужие/битые пакеты вернут Err → None.
        // Никакой ручной валидации nonce не нужно.
        self.cipher.decrypt(Nonce::from_slice(nonce), ct).ok()
    }
}
```

## Заметки по tap-windows6 (чтобы агент не гадал)

- Открытие: `CreateFileW(r"\\.\Global\{GUID}.tap", GENERIC_READ|GENERIC_WRITE, 0, null, OPEN_EXISTING, FILE_ATTRIBUTE_SYSTEM|FILE_FLAG_OVERLAPPED, null)`.
- Поднять линк: `DeviceIoControl` с IOCTL `TAP_IOCTL_SET_MEDIA_STATUS`, входной буфер `[1u32]` (connected).
- Чтение/запись: overlapped `ReadFile`/`WriteFile`, ждать через `GetOverlappedResult`. Целые Ethernet-фреймы.
- IOCTL-коды: `CTL_CODE(FILE_DEVICE_UNKNOWN, function, METHOD_BUFFERED, FILE_ANY_ACCESS)` — вынести в константы с комментарием, откуда они.
