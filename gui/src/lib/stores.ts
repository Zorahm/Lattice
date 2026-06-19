// Состояние приложения — Svelte stores, реактивно обновляются из backend-
// событий (см. bridge.ts). Фронт тонкий: здесь только зеркало состояния,
// никакой сетевой/крипто-логики.

import { writable, derived } from "svelte/store";
import {
  DEFAULT_SETTINGS,
  type ConnectionStatus,
  type Diagnostics,
  type Peer,
  type Settings,
} from "./types";

export type Screen = "login" | "connected" | "settings";

/** Текущий экран. Настройки доступны из любого экрана. */
export const screen = writable<Screen>("login");

/** Куда вернуться по «Назад» из настроек. */
export const settingsReturn = writable<Screen>("login");

export const status = writable<ConnectionStatus>({ phase: "disconnected" });

export const peers = writable<Peer[]>([]);

export const diagnostics = writable<Diagnostics>({});

export const settings = writable<Settings>(structuredClone(DEFAULT_SETTINGS));

/** Поля формы входа (для чекбокса «Запомнить»). */
export const loginForm = writable<{ network: string; password: string; remember: boolean }>(
  { network: "", password: "", remember: false },
);

/** Удобные производные флаги. */
export const isBusy = derived(status, ($s) =>
  $s.phase === "connecting" || $s.phase === "reconnecting",
);

export const isOnline = derived(status, ($s) => $s.phase === "connected");

export function openSettings(from: Screen): void {
  settingsReturn.set(from);
  screen.set("settings");
}
