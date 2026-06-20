// Тексты интерфейса и маппинг ошибок в человеческий язык (спека: «что
// произошло + что делать», без errno и жаргона). Пока только русский —
// язык в настройках заложен на будущее, но строки здесь одни.

import type { AppError, ErrorKind, LinkState } from "./types";

export const t = {
  appName: "Lattice",

  // Экран 1 — вход
  networkLabel: "Название сети",
  passwordLabel: "Пароль",
  networkPlaceholder: "напр. my-home-lan",
  loginHint: "Тот, кто введёт такие же название и пароль, попадёт в эту же сеть.",
  connect: "Подключиться",
  connecting: "Подключение…",
  remember: "Запомнить",
  showPassword: "Показать пароль",
  hidePassword: "Скрыть пароль",
  settings: "Настройки",

  // Экран 2 — в сети
  you: "вы",
  disconnect: "Отключиться",
  reconnecting: "Переподключение…",
  connected: "В сети",
  emptyPeers: "Поделитесь названием и паролем, чтобы пригласить других.",
  legendDirect: "напрямую",
  legendRelay: "ретранслятор",
  legendOffline: "офлайн",
  offline: "офлайн",
  online: "В сети",
  peersCount: (n: number) => (n === 1 ? "1 участник" : `${n} участников`),

  // Комнаты (Radmin-стиль)
  myRooms: "Мои комнаты",
  noRoomsTitle: "Вы пока ни в одной комнате",
  noRoomsHint: "Создайте свою комнату или присоединитесь к существующей по названию и паролю.",
  createRoom: "Создать комнату",
  joinRoom: "Присоединиться",
  create: "Создать",
  join: "Войти",
  cancel: "Отмена",
  roomNameLabel: "Название комнаты",
  roomConnect: "Подключиться",
  roomConnecting: "Подключение…",
  roomConnected: "Подключена",
  roomDisconnected: "Не подключена",
  roomLeave: "Покинуть комнату",
  roomLeaveConfirm: (n: string) => `Удалить комнату «${n}» из списка?`,
  roomActive: "активна",
  editName: "Изменить имя",
  editNameHint: "Это имя увидят другие участники комнаты.",

  // Контекстное меню пира / свойства
  ctxCopyIp: "Копировать IP-адрес",
  ctxCopyName: "Копировать имя",
  ctxProperties: "Свойства",
  copiedShort: "Скопировано",
  propTitle: "Свойства узла",
  propName: "Имя",
  propOverlayIp: "Overlay-IP",
  propLink: "Связь",
  propPing: "Задержка",
  close: "Закрыть",
  self: "Это устройство",

  // Экран 3 — настройки
  settingsTitle: "Настройки",
  back: "Назад",
  secNetwork: "Сеть",
  secServer: "Сервер координации",
  secConnection: "Соединение",
  secApp: "Приложение",
  secDiagnostics: "Диагностика",

  fSubnet: "Подсеть",
  fIpAssign: "Назначение IP",
  fIpAuto: "Автоматически",
  fIpManual: "Вручную",
  fOverlayIp: "Overlay-IP",
  fMtu: "MTU",
  fMtuHint: "не трогать без необходимости",
  fCoordination: "Адрес сервера",
  fStun: "STUN-серверы",
  fStunCount: (n: number) => `${n} по умолчанию`,
  fAllowRelay: "Разрешить ретранслятор",
  fListenPort: "Порт прослушивания",
  fPortAuto: (p: number) => `авто (${p})`,
  fKeepalive: "Интервал keepalive",
  fAdvanced: "расширенные ›",
  fAutostart: "Запуск с Windows",
  fMinimizeToTray: "Сворачивать в трей",
  fLanguage: "Язык",
  fLangRu: "Русский",
  fNatType: "Тип NAT",
  fExternalEndpoint: "Внешний endpoint",
  copyLog: "Скопировать лог",
  copied: "Скопировано",
  unknown: "—",
  save: "Сохранить",
  saved: "Сохранено",
  seconds: "с",
};

/** Подпись тултипа индикатора связи (без жаргона NAT/punching на экране). */
export function linkTitle(link: LinkState): string {
  switch (link) {
    case "p2p":
      return t.legendDirect;
    case "relay":
      return t.legendRelay;
    case "offline":
      return t.legendOffline;
  }
}

/** Маппинг ошибки backend в человеческий текст (заголовок + что делать). */
export function errorMessage(err: AppError | null | undefined): {
  title: string;
  action: string;
} {
  const kind: ErrorKind = err?.kind ?? "unknown";
  switch (kind) {
    case "not_admin":
      return {
        title: "Нужны права администратора",
        action: "Запустите от имени администратора — нужно для адаптера.",
      };
    case "no_tap_driver":
      return {
        title: "Не найден сетевой драйвер",
        action: "Установите TAP-драйвер и попробуйте снова.",
      };
    case "server_unreachable":
      return {
        title: "Не удаётся связаться с сервером",
        action: "Проверьте интернет или адрес сервера в настройках.",
      };
    case "bad_input":
      return {
        title: "Проверьте введённые данные",
        action: "Заполните название сети и пароль.",
      };
    default:
      return {
        title: "Что-то пошло не так",
        action: "Попробуйте ещё раз. Детали — в Диагностике.",
      };
  }
}
