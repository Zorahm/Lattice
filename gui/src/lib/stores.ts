// Состояние приложения — Svelte stores, реактивно обновляются из backend-
// событий (см. bridge.ts). Фронт тонкий: здесь только зеркало состояния,
// никакой сетевой/крипто-логики.

import { writable, derived } from "svelte/store";
import {
  DEFAULT_SETTINGS,
  type ConnectionStatus,
  type Diagnostics,
  type Peer,
  type Room,
  type Settings,
} from "./types";

export type Screen = "rooms" | "settings";

/** Текущий экран. Хаб комнат — главный; настройки доступны из него. */
export const screen = writable<Screen>("rooms");

/** Куда вернуться по «Назад» из настроек. */
export const settingsReturn = writable<Screen>("rooms");

export const status = writable<ConnectionStatus>({ phase: "disconnected" });

export const peers = writable<Peer[]>([]);

export const diagnostics = writable<Diagnostics>({});

export const settings = writable<Settings>(structuredClone(DEFAULT_SETTINGS));

// --- Комнаты (Radmin-стиль: список сохранённых, активна одна за раз) ---------

const ROOMS_KEY = "lattice.rooms";
const SELF_NAME_KEY = "lattice.selfName";

/** Сохранённые комнаты. Персистятся в localStorage (как и логин раньше). */
export const rooms = writable<Room[]>([]);

/** id комнаты, к которой сейчас подключаемся/подключены (или null). */
export const activeRoomId = writable<string | null>(null);

/** Своё отображаемое имя (видно всем в комнате). Пустое = hostname с backend. */
export const selfName = writable<string>("");

/** Загрузить комнаты и своё имя из localStorage (вызывать на старте). */
export function loadRooms(): void {
  if (typeof window === "undefined") return;
  try {
    const raw = window.localStorage.getItem(ROOMS_KEY);
    if (raw) rooms.set(JSON.parse(raw) as Room[]);
  } catch {
    /* битый кеш — игнорируем */
  }
  selfName.set(window.localStorage.getItem(SELF_NAME_KEY) ?? "");
}

function persistRooms(list: Room[]): void {
  if (typeof window === "undefined") return;
  window.localStorage.setItem(ROOMS_KEY, JSON.stringify(list));
}

export function persistSelfName(name: string): void {
  selfName.set(name);
  if (typeof window !== "undefined") {
    window.localStorage.setItem(SELF_NAME_KEY, name);
  }
}

function newId(): string {
  return `r-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 7)}`;
}

/** Добавить комнату (создать/присоединиться). Возвращает её. Дубль по имени
 *  переиспользуется, чтобы список не плодил одинаковые сети. */
export function addRoom(name: string, password: string): Room {
  let result!: Room;
  rooms.update((list) => {
    const existing = list.find((r) => r.name === name);
    if (existing) {
      existing.password = password;
      result = existing;
      persistRooms(list);
      return list;
    }
    result = { id: newId(), name, password };
    const next = [...list, result];
    persistRooms(next);
    return next;
  });
  return result;
}

export function removeRoom(id: string): void {
  rooms.update((list) => {
    const next = list.filter((r) => r.id !== id);
    persistRooms(next);
    return next;
  });
}

/** Удобные производные флаги. */
export const isBusy = derived(status, ($s) =>
  $s.phase === "connecting" || $s.phase === "reconnecting",
);

export const isOnline = derived(status, ($s) => $s.phase === "connected");

export function openSettings(from: Screen): void {
  settingsReturn.set(from);
  screen.set("settings");
}
