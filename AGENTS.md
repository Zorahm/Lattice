# AGENTS.md — Lattice

Source of truth: `SPEC.md`. This file fixes non-negotiable quality bar and
architectural decisions that must hold across all phases.

## Архитектурные решения (не менять без явного пересмотра SPEC)

- **L2, не L3.** Виртуальный адаптер — `tap-windows6` (OpenVPN), не Wintun.
  Причина: только L2 даёт настоящий Ethernet-broadcast/multicast → LAN-discovery
  для игр (Minecraft LAN, SA:MP, Project Zomboid) работает из коробки. Wintun —
  L3, ARP/broadcast эмулировать вручную — не берём.
- **No WireGuard.** Не используем WG / boringtun / wireguard-go. Кастомный
  UDP-датаплейн + ChaCha20-Poly1305, чтобы не иметь известной DPI-сигнатуры.
- **Сменяемый транспорт.** Датаплейн за `trait Transport`, discovery за
  `trait Discovery`. Crypto и TAP от транспорта не зависят. Фаза 2 меняет
  static→STUN, Фаза 4 меняет UDP→QUIC — без переписывания crypto/tap.
- **client/server не делят платформенные зависимости.** `lattice-server` не
  тянет `windows` crate даже транзитивно. Платформенное — только в
  `lattice-client`. Общее — только в `lattice-proto` (чистый serde, no_std-
  friendly по возможности).
- **FFI-изоляция.** Весь `unsafe` / Win32 — только в `lattice-client/src/tap/`
  (модуль `tap` с подмодулями `mod.rs`/`registry.rs`/`win32.rs`). Больше
  `unsafe` нигде. Сервер Win32 не касается в принципе.
- **MTU ~1380.** После инкапсуляции (nonce 12 + AEAD-тег 16 + UDP 8 + IP 20 ≈
  56 байт оверхеда) датаграмма не должна превышать 1500 → TAP MTU ~1380 против
  фрагментации.
- **Стек:** Rust edition 2021, target `x86_64-pc-windows-msvc`, tap-windows6,
  `chacha20poly1305`, UDP. Не подменять.

## Фазы (текущая — Фаза 3)

- **Фаза 1 — PoC.** Только `lattice-client` (+ `lattice-proto` почти пустой,
  `lattice-server` — плейсхолдер). Критерий: две Win-машины пингуют друг друга
  по 10.66.0.0/24, Wireshark на TAP видит ARP-broadcast.
- **Фаза 2 — NAT traversal.** STUN + UDP hole punching + relay-fallback, сведены
  минимальным rendezvous-сервером. За `trait Discovery`/`trait Transport`,
  Фаза 1 не переписана (статический `--peer` — отдельная ветка).
- **Фаза 3 — coordination-сервер.** `lattice-server` раскрыт в полный
  control-plane: mesh на N узлов, реестр пиров по сетям (network-id → список),
  динамический join/leave/heartbeat, relay на сеть, REST API + статический WebUI
  (localhost-only). Фаза 2 (комнатный rendezvous на 2 пиров) не сломана —
  mesh-режим идёт отдельным набором control-сообщений (`lattice_proto::mesh`).
- **Фаза 4 — обфускация транспорта.** QUIC (`quinn`) как маскирующая обёртка под
  HTTP/3 за тем же `trait Transport` + padding длин и timing jitter. Только в
  клиенте (сервер без tokio/quinn — контракт сохранён). Фазы 1-3 (голый UDP) не
  сломаны: транспорт выбирается флагом `--transport auto|udp|quic`, дефолт `auto`.

## Архитектурные решения Фазы 2 (NAT traversal)

- **Rendezvous минимален.** Сервер Фазы 2 делает ровно две вещи: сводит ДВУХ
  пиров по `room-id` (control-канал) и ретранслирует датаплейн при провале punch
  (relay). Без реестра пиров, mesh > 2, персистентности, раздачи ключей, WebUI —
  это Фаза 3. Ключ на Фазе 2 всё ещё shared (`--key`).
- **Сервер на std-потоках, не tokio.** Контракт «no windows crate даже
  транзитивно» (деплой Linux): `tokio` на Windows-хосте транзитивно тянет
  `windows-sys` (mio), и `cargo tree -p lattice-server` на dev-Windows это
  показал бы. `std::net` использует биндинги libstd, не crate `windows`. Для
  2 пиров блокирующих потоков хватает; tokio+axum придут на Фазе 3. По той же
  причине `env_logger` в сервере — с `default-features = false` (его цветной
  вывод через `anstream` тянет `windows-sys`).
- **Один UDP-сокет на STUN + punch + data.** Внешний NAT-маппинг привязан к
  локальному (ip:port). STUN, hole punching и датаплейн идут через ОДИН сокет —
  иначе srflx, узнанный по STUN, не совпал бы с маппингом данных и punch
  промахнулся бы. Control-канал (сигналинг) — отдельный TCP, чтобы не
  демультиплексировать STUN/punch/data на одном порту.
- **Control-канал — TCP, relay — UDP.** Регистрация → матч → синхронный
  go-сигнал требуют надёжной упорядоченной доставки → TCP (length-delimited JSON,
  `lattice_proto::framing`, фича `std`). Датаплейн и relay — UDP (lossy ок для
  VPN-датапути, минимум оверхеда).
- **NAT-эвристика: symmetric → relay.** Сравниваем srflx-маппинги на ДВА разных
  STUN-таргета. Разный внешний порт ⇒ симметричный NAT ⇒ srflx бесполезен пиру ⇒
  сервер сразу назначает relay, не тратя секунды на обречённый punch. Одинаковый
  порт (cone) ⇒ punch. STUN недоступен ⇒ деградация в relay (помечаем как
  symmetric), не падаем.
- **Punch корректный, не наивный.** Оба пира шлют bursts одновременно по
  go-сигналу (NAT пропускает входящий UDP только после исходящего). Несколько
  попыток с потолком по времени (не вечный цикл) → при таймауте relay. Punch-
  пакеты шифруются тем же AEAD (аутентификация против угона + демультиплексирование
  по `CTRL_MAGIC` в расшифрованном payload). Keepalive (~20с, ниже NAT-таймаута
  30-60с) держит маппинг живым; watchdog по тишине → переустановка через
  rendezvous (не молчаливая потеря связи).
- **Relay видит только ciphertext.** E2E не ослабляется: в relay-обёртку
  (`[magic][session][payload]`) кладётся уже зашифрованная датаграмма
  `[nonce || AEAD(frame)]`; сервер ретранслирует по `session` + source-адресу,
  ключа не имеет и расшифровать не может.
- **Сменяемый транспорт держит контракт.** Direct (`UdpTransport`) и relay
  (`RelayTransport`) — обе реализации `trait Transport`; датаплейн-циклы
  (`session.rs`) не знают, какой транспорт под ними. Static/direct/relay гоняются
  одним кодом.

## Архитектурные решения Фазы 3 (coordination-сервер)

- **Крипто-модель не меняется.** Сервер в датаплейн-крипто НЕ лезет: ключа сети
  не видит и не раздаёт. E2E остаётся симметричным shared-key из Фаз 1-2.
  «Сеть» идентифицируется `network-id = BLAKE3(shared-key)` (32 байта); клиент
  вычисляет его локально (где ключ живёт) и предъявляет серверу только хэш.
  Сервер сводит пиров с одинаковым `network-id`. Relay по-прежнему видит только
  ciphertext. В коде сервера ключ сети физически недоступен — его там нет.
- **In-memory persistence — осознанно.** Состояние реестра живёт в RAM и
  теряется при рестарте сервера. Это ОК: клиенты при потере связи переподключаются
  и перерегистрируются (реестр восстанавливается из живых клиентов), не виснут
  мёртвыми. SQLite/персистентность — за рамками Фазы 3; реестр за `trait
  Registry`, чтобы добавить хранилище потом без переписывания callers.
- **Mesh-туннели — по одному на пира.** В mesh N узлов клиент держит N-1
  одновременных p2p-путей (direct или relay), по одному на пира; fallback на
  relay — per-pair (часть пар напрямую, часть на relay — честный degraded-статус).
  Relay-сессия = на сеть (один `session` на сеть, relay пересылает каждому кроме
  отправителя): L2-overlay рассылает фреймы всем пирам, это совпадает с
  broadcast-моделью TAP и не требует per-pair session.
- **Сервер остаётся на std-потоках, не tokio.** Контракт «no windows crate даже
  транзитивно» (см. Фаза 2) держится и для Фазы 3: `tokio` на Windows-хосте тянет
  `windows-sys` через `mio`. HTTP API + WebUI реализованы на голом
  `std::net::TcpListener` с минимальным ручным разбором HTTP/1.1 — без hyper/axum,
  чтобы не тянуть рантайм. WebUI — статический HTML+JS (без сборочного пайплайна),
  раздаётся тем же HTTP-сервером.
- **Админка localhost-only по умолчанию.** WebUI/API биндится на `127.0.0.1`
  отдельным портом от signaling/relay. Торчит наружу ТОЛЬКО при явном флаге
  `--web-expose` (и тогда `--web-bind 0.0.0.0`). Запрос с не-localhost без флага
  → отказ, не молчаливое выставление админки. Аутентификации/мультитенантности
  нет осознанно (это не публичный SaaS).
- **Mesh-режим = отдельный набор control-сообщений.** Чтобы не сломать Фазу 2
  (комнатный rendezvous на 2 пиров), mesh-протокол живёт в `lattice_proto::mesh`
  (`MeshClientMessage`/`MeshServerMessage`), а не расширяет `control`.
  Сервер обслуживает оба режима на одном control-TCP-листенере: первое сообщение
  решает, room или mesh. `PROTOCOL_VERSION` инкрементирован до 3.
- **Heartbeat presence: разумный порог, не одна потеря.** Клиент шлёт heartbeat
  каждые ~15с; сервер помечает пир offline только после 3 пропусков (~45с), не
  при первом молчании — разовые потери UDP/TCP-лаг не выкидывают пира. Протухший
  пир → `PeerLeft` остальным, запись удаляется.
- **Коллизия overlay-IP детектируется сервером.** Два пира в одной сети с
  одинаковым self-назначенным overlay-IP → сервер сигналит `Error` второму
  (не молчаливый конфликт в датаплейне). Overlay-IP — self-assigned клиентом,
  сервер хранит для отображения и проверки уникальности в сети.

## Архитектурные решения Фазы 4 (обфускация транспорта)

- **Честная планка — не переобещать.** Цель: НЕ матчиться по сигнатуре известных
  VPN и НЕ выделяться пассивной эвристикой «непонятный шифрованный UDP». Цель НЕ
  в криптографической неотличимости от настоящего HTTP/3 под АКТИВНЫМ пробингом —
  этого не гарантируем и не заявляем. В коде/доке формулировка: «не матчится по
  сигнатуре и пассивной эвристике; против активного пробинга не тестировалось».
  Это гонка, а не решённая задача — обёртки сменяемы за `trait Transport` дёшево.
- **QUIC — только в клиенте, сервер без tokio/quinn.** `quinn` тянет `tokio`
  (→ `windows-sys` через `mio` на Windows-хосте) и `rustls`. Контракт «сервер не
  тянет windows crate» (Фазы 2-3) сохранён: QUIC живёт ТОЛЬКО в `lattice-client`
  (Windows-only, уже тянет `windows-sys` — для него tokio не нарушение). Сервер
  остаётся на std-потоках; `cargo tree -p lattice-server` по-прежнему без
  `windows`/`tokio`/`quinn`/`rustls`. QUIC-датаплейн — прямой p2p (после punch),
  сервер в нём не участвует. `ring` (не `aws-lc-rs`) как crypto-backend — чистый
  Rust+asm, без CMake/NASM, собирается под `x86_64-pc-windows-msvc`.
- **Двойное шифрование — осознанно.** Внутрь QUIC едут УЖЕ зашифрованные
  ChaCha20-Poly1305 датаграммы (E2E shared-key Фаз 1-3). QUIC — ВНЕШНИЙ слой РАДИ
  МАСКИРОВКИ, не ради защиты. Серверный QUIC-сертификат НЕ проверяется
  (`AcceptAnyServerCert`): доверие даёт внутренний ChaCha-слой, у пиров нет
  PKI/CA, а подделанный QUIC-cert не открывает внутренний слой. Relay по-прежнему
  видит только ciphertext внутреннего слоя.
- **QUIC DATAGRAM, не стримы.** Датаплейн в QUIC DATAGRAM-фреймах (RFC 9221) —
  UDP-семантика без HOL-blocking; надёжные стримы ломали бы latency игр/видео
  ретрансмитами поверх уже lossy-tolerant трафика.
- **Sync↔async мост.** Остальной код синхронный (std-потоки). QUIC-соединение
  живёт в фоновом tokio-рантайме (свой поток), `send`/`recv` общаются через
  каналы; `recv` — `recv_timeout` (опрос shutdown, как у `UdpTransport`).
  `QuicTransport: Sync` (Mutex вокруг std-Receiver), чтобы делиться `&` между
  потоками сессии. Handshake ограничен явным таймаутом — зарезанный порт/SNI
  падает быстро, selector сразу откатывается, а не висит до idle-таймаута QUIC.
- **Выбор транспорта — явная машина состояний (`selector`), не каскад if.**
  `auto` пробует голый UDP первым (дешевле: ноль QUIC/TLS-оверхеда, ниже latency,
  прямой p2p); при неуспехе эскалирует на QUIC (HTTP/3-мимикрия). Защита от
  зацикливания: один цикл = UDP+QUIC, между циклами backoff, число циклов
  ограничено → `Exhausted` (явный отказ, не вечный ретрай). Роли QUIC (кто
  listener, кто client) — детерминированно по `peer-id`, которые оба пира уже
  знают из mesh: меньший слушает. Без нового signaling-сообщения и без изменений
  сервера — обе стороны независимо приходят к одному распределению (нет
  молчаливого рассинхрона «обе слушают»).
- **Маскирующие меры конфигурируемы, с ценой в комментарии.** `--obfs-padding`
  добивает мелкие датаграммы до типичного размера (ломает распределение длин;
  cap на оверхед, крупное не паддит); меняет wire-формат → ОБА пира включают,
  применяется только к direct-путям (relay держит конвенцию «пустой payload =
  hello»). `--obfs-jitter` рандомизирует каденцию keepalive/heartbeat (ломает
  машинный ритм; для датаплейна осторожнее — jitter только на служебных пакетах,
  не на каждом фрейме). Оба дефолтно ВЫКЛ → Фазы 1-3 байт-в-байт не меняются.
- **MTU при QUIC.** QUIC+DATAGRAM-заголовки отъедают поверх и так тесного 1380.
  При forced-QUIC TAP MTU опускается на `QUIC_DATAGRAM_OVERHEAD` (`transport::
  quic_effective_mtu`) — иначе крупные фреймы тихо дропались бы (датаграмма
  больше `max_datagram_size` не уходит). При `auto` MTU пересчитывается на
  эскалации (часть установления), не статичен.
- **Что НЕ делаем (Фаза 4).** Свой обфускатор-протокол/крипту не пишем (берём
  QUIC/TLS как есть). Датаплейн-крипто не трогаем. Domain-fronting/Reality
  (маскировка под ЧУЖОЙ сайт) — отдельная тема, не сюда (но `trait Transport`
  позволяет добавить потом). Гарантий обхода конкретного DPI/РКН не заявляем.

## Стандарт качества (Rust)

- `clippy::pedantic` без массового игнора; компиляция **без warnings**.
- `unwrap()`/`expect()` **запрещены в горячем пути** (tap/crypto/transport
  циклы). `expect` допустим только на старте, где инвариант доказан
  комментарием-«почему».
- `unsafe` только в `lattice-client/src/tap/` (FFI-граница с драйвером и
  общие Win32-помощники: admin-check, console-handler).
- **Newtype-обёртки** для семантически разных значений: ключ, nonce — не голые
  `[u8]`. IOCTL-коды — именованные константы с комментарием, откуда взяты
  (tap-windows6 `tap.h`, `CTL_CODE` макрос).
- Ошибки через `thiserror` (свой enum с `Display`) + проброс через `?`. Win32-
  ошибки оборачивать в осмысленный контекст, не пробрасывать голый код.
- Crypto `open()` полагается на AEAD-тег для отсева чужих/битых пакетов —
  никакой ручной валидации nonce.
- Один модуль — одна ответственность. Файлы ≤ 300 строк. Функция — на экран.
- Комментарии объясняют **«почему»** (L2 vs L3, MTU 1380, источник IOCTL-кодов),
  не «что».
- Не комментировать код без необходимости (см. выше — только «почему»).

## Edge cases (обрабатывать явно, не глотать)

- датаграмма < 12 байт (нет nonce) → дроп
- AEAD-тег не сошёлся → дроп молча, без паники
- TAP overlapped read/write ошибка → лог + продолжить цикл
- UDP sendfail (пир недоступен) → лог, не падать
- невалидный `--key` (не 32 байта hex) → внятная ошибка при старте
- запуск без прав администратора → внятное сообщение и выход
- tap-windows6 не установлен / адаптер не найден → ошибка с подсказкой
  поставить драйвер, не сырой Win32 errno

### Фаза 2 (NAT traversal)

- STUN недоступен/таймаут → резервный STUN-таргет, затем деградация в relay
  (помечаем NAT как symmetric), не падаем
- rendezvous недоступен → внятная ошибка (`SignalError::Connect`), не паника
- punch не сошёлся за таймаут → переход на relay, лог причины
- пир отвалился (control `PeerGone` / обрыв TCP) → корректное завершение сессии
- relay/control-канал разорвался → `ControlLost`, внятный лог, выход
- NAT-биндинг протух (тишина дольше watchdog-таймаута) → переустановка через
  rendezvous (`SessionEnd::LinkDead`), не молчаливая потеря связи
- сервер: отравленный mutex / занятая комната / несовпадение версии протокола →
  `ServerMessage::Error`, не паника

### Фаза 3 (coordination-сервер)

- два пира в одной сети с одинаковым self-назначенным overlay-IP → сервер
  сигналит `Error` второму (коллизия детектируется, не молчаливый конфликт)
- пир переподключается с новым endpoint (сменилась сеть/NAT) → реестр обновляет
  endpoint по `peer_id`, шлёт `PeerUpdated` остальным, не дубль-запись
- сервер рестартнул → клиенты переподключаются по таймауту heartbeat и
  перерегистрируются, mesh восстанавливается из живых, не вечное ожидание
- mesh частично degraded (часть пар напрямую, часть на relay, часть недоступна)
  → честный per-pair статус в WebUI, не «всё или ничего»
- presence-таймаут vs временный лаг → пир помечается offline только после 3
  пропущенных heartbeat (~45с), не при первой потере
- WebUI запрошен с не-localhost без `--web-expose` → 403/отказ, не молчаливое
  выставление админки наружу
- сервер: отравленный mutex / неизвестный network-id / коллизия overlay-IP →
  `MeshServerMessage::Error`, не паника

### Фаза 4 (обфускация транспорта)

- QUIC-handshake не прошёл (порт/SNI зарезан) → `QuicError::Handshake` с явным
  таймаутом, лог, selector откатывается/эскалирует, не висим до idle-таймаута
- битый `--sni` → fail-fast на старте (`announce_transport` собирает QUIC-конфиг
  до запуска воркеров), внятная ошибка, не падение при эскалации в бою
- auto-эскалация не зацикливается → машина состояний с потолком циклов и backoff,
  исчерпание → `Exhausted` (явный отказ)
- фрейм больше QUIC `max_datagram_size` → `send` возвращает явную ошибку (caller
  логирует), не тихий дроп/фрагментация; TAP MTU опущен на QUIC-оверхед
- padding меняет wire-формат → применяется только к direct-путям и только при
  `--obfs-padding` у ОБОИХ пиров; relay-путь держит конвенцию «пустой payload =
  hello» (obfs там не применяется), без молчаливого рассинхрона
- одна сторона listener, другая client (QUIC асимметричен) → роли детерминированы
  по `peer-id` (меньший слушает), обе стороны согласованы без нового signaling
- QUIC recv-mutex отравлен → `TransportError::Io`, не паника в горячем пути

## Сборка/проверка

- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo check --workspace` / `cargo test --workspace`
- Клиент собирается под `x86_64-pc-windows-msvc` (Windows-хост).
- `lattice-proto` собирается без платформенных зависимостей (по умолчанию
  `no_std + alloc`; фича `std` включает только `framing` поверх `std::io`).
- `cargo tree -p lattice-server` не должен содержать `windows` (отсюда: сервер
  без tokio, `env_logger` без default-фич). Фаза 4: `quinn`/`tokio`/`rustls`/
  `ring` появляются ТОЛЬКО в `lattice-client` — `cargo tree -p lattice-server`
  и `-p lattice-proto` по-прежнему без них (проверено).
- Реальный punch за двумя живыми NAT агент не воспроизводит — проверяется в бою;
  кроссплатформенная логика сервера покрыта `tests/rendezvous.rs`. Фаза 4: QUIC
  покрыт loopback-тестами (handshake + DATAGRAM roundtrip + socket-reuse +
  fail-fast на мёртвый порт), obfs/selector — unit-тестами; реальная проверка
  против живого DPI и QUIC-эскалация за живым NAT — в бою (гарантий обхода не
  заявляем).

## Структура workspace

```
lattice/
├── Cargo.toml            # workspace, resolver = "2"
├── AGENTS.md             # этот файл
├── SPEC.md               # спецификация (источник истины)
└── crates/
    ├── lattice-client/   # Windows-only датаплейн + кроссплатформенные сетевые модули Фазы 2/3:
    │                     #   main/cli/run, crypto, tap/{mod,registry,win32}, transport, netcfg,
    │                     #   peers (Static/Dynamic/Mesh Discovery), stun, signaling, punch, relay,
    │                     #   dynamic (establish), session (циклы + watchdog), network_id (BLAKE3),
    │                     #   mesh (join/list/punch-per-peer/heartbeat/reconnect);
    │                     #   transport/{quic,quic_tls,obfs,selector} — Фаза 4 (QUIC/h3, padding/jitter,
    │                     #   машина выбора auto/udp/quic) — только клиент, сервер без tokio/quinn
    ├── lattice-server/   # кроссплатформенный coordination-сервер (std-потоки): main/lib,
    │                     #   control (room TCP Фаза 2), rooms (Фаза 2), relay (UDP, session на сеть),
    │                     #   registry (trait + InMemoryRegistry), presence (heartbeat cleanup),
    │                     #   mesh_control (mesh TCP), web (REST + статика), http (ручной HTTP/1.1);
    │                     #   tests/rendezvous.rs (Фаза 2), tests/registry.rs, tests/mesh.rs
    └── lattice-proto/    # shared типы: control (room-сообщения Фаза 2), mesh (сообщения Фаза 3),
                          #   relay (обёртка), framing (std), ids (NetworkId/PeerId/OverlayIp newtype)
```
