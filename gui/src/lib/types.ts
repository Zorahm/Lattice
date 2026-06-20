// Контракт фронт ↔ backend. Эти типы должны совпадать с сериализацией
// Tauri-команд и событий в gui/src-tauri/src/lib.rs (serde camelCase).

/** Состояние связи с конкретным пиром (цвет индикатора в списке). */
export type LinkState = "p2p" | "relay" | "offline";

/** Один пир сети — как приходит в событии `peers`. */
export interface Peer {
  /** Стабильный id пира (ключ списка). */
  id: string;
  /** Отображаемое имя (peer-id / hostname). */
  name: string;
  /** Overlay-IP в виртуальной сети, напр. 10.66.0.2. */
  overlayIp: string;
  /** Тип связи → цвет индикатора. */
  link: LinkState;
  /** Пинг в мс, если известен. */
  pingMs?: number | null;
}

/** Сохранённая комната (сеть) — Radmin-стиль: список, активна одна за раз. */
export interface Room {
  /** Стабильный локальный id (ключ списка), не зависит от имени. */
  id: string;
  /** Имя сети — доменный разделитель в KDF, видно как «название комнаты». */
  name: string;
  /** Пароль сети (хранится локально, как чекбокс «Запомнить»). */
  password: string;
}

/** Жизненный цикл подключения — как приходит в событии `status`. */
export type ConnectionPhase =
  | "disconnected"
  | "connecting"
  | "connected"
  | "reconnecting"
  | "error";

export interface ConnectionStatus {
  phase: ConnectionPhase;
  /** Название сети, к которой подключаемся/подключены. */
  network?: string;
  /** Свой overlay-IP (без префикса), напр. 10.66.0.1. */
  overlayIp?: string;
  /** Имя этого узла (hostname) — для карточки «себя». */
  selfName?: string;
  /** Человеческий текст ошибки (только при phase === "error"). */
  error?: AppError | null;
}

/** Категория ошибки — для маппинга в человеческий текст (см. i18n). */
export type ErrorKind =
  | "not_admin"
  | "no_tap_driver"
  | "server_unreachable"
  | "bad_input"
  | "unknown";

export interface AppError {
  kind: ErrorKind;
  /** Технические детали (для Диагностики/лога), не для главного экрана. */
  detail?: string;
}

/** Диагностика — приходит событием `diagnostics`. */
export interface Diagnostics {
  natType?: string;
  externalEndpoint?: string;
}

export type Theme = "dark" | "light";

/** Настройки приложения. Пустые = работает из коробки (рабочие дефолты). */
export interface Settings {
  network: {
    subnet: string;
    /** Назначение overlay-IP: авто или вручную. */
    ipAssign: "auto" | "manual";
    /** Конкретный overlay-IP в CIDR (используется backend как --tap-ip). */
    overlayIp: string;
    mtu: number;
  };
  server: {
    /** Адрес сервера координации host[:port]. */
    coordination: string;
    /** STUN-серверы. */
    stun: string[];
  };
  connection: {
    allowRelay: boolean;
    /** 0 = авто. */
    listenPort: number;
    keepaliveSecs: number;
  };
  app: {
    autostart: boolean;
    minimizeToTray: boolean;
    language: "ru" | "en";
  };
}

export const DEFAULT_SETTINGS: Settings = {
  network: {
    subnet: "10.66.0.0/24",
    ipAssign: "auto",
    overlayIp: "10.66.0.1/24",
    mtu: 1380,
  },
  server: {
    coordination: "lattice.zorahm.ru:51821",
    stun: ["stun.l.google.com:19302", "stun.cloudflare.com:3478"],
  },
  connection: {
    allowRelay: true,
    listenPort: 0,
    keepaliveSecs: 15,
  },
  app: {
    autostart: false,
    minimizeToTray: true,
    language: "ru",
  },
};
