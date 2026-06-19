// Единственный мост к Rust. Вся сеть/крипто — в backend; здесь только вызовы
// Tauri-команд и подписка на события, которые обновляют stores.
//
// В обычном браузере (vite dev без Tauri) Tauri-API отсутствует — тогда
// работает лёгкий mock, чтобы UI можно было разрабатывать и смотреть. В сборке
// Tauri используется настоящий backend.

import { peers, status, diagnostics, settings, loginForm } from "./stores";
import {
  DEFAULT_SETTINGS,
  type ConnectionStatus,
  type Diagnostics,
  type Peer,
  type Settings,
} from "./types";

interface TauriCore {
  invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T>;
}
interface TauriEvent {
  listen<T>(
    event: string,
    handler: (e: { payload: T }) => void,
  ): Promise<() => void>;
}

function hasTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

async function core(): Promise<TauriCore> {
  return (await import("@tauri-apps/api/core")) as unknown as TauriCore;
}
async function events(): Promise<TauriEvent> {
  return (await import("@tauri-apps/api/event")) as unknown as TauriEvent;
}

// --- Команды ---------------------------------------------------------------

export interface ConnectArgs {
  network: string;
  password: string;
}

export async function connect(args: ConnectArgs): Promise<void> {
  if (!args.network || !args.password) {
    status.set({ phase: "error", error: { kind: "bad_input" } });
    return;
  }
  if (!hasTauri()) return mockConnect(args);
  status.update((s) => ({ ...s, phase: "connecting", network: args.network }));
  const c = await core();
  // Пароль уходит в backend как есть; KDF→ключ→network-id считает Rust.
  await c.invoke("connect", { network: args.network, password: args.password });
}

export async function disconnect(): Promise<void> {
  if (!hasTauri()) return mockDisconnect();
  const c = await core();
  await c.invoke("disconnect");
}

export async function loadSettings(): Promise<Settings> {
  if (!hasTauri()) {
    settings.set(structuredClone(DEFAULT_SETTINGS));
    return structuredClone(DEFAULT_SETTINGS);
  }
  const c = await core();
  const s = await c.invoke<Settings>("get_settings");
  settings.set(s);
  return s;
}

export async function saveSettings(s: Settings): Promise<void> {
  settings.set(s);
  if (!hasTauri()) return;
  const c = await core();
  await c.invoke("save_settings", { settings: s });
}

/** Вернуть текст лога для копирования (фронт сам кладёт в буфер обмена). */
export async function getLog(): Promise<string> {
  if (!hasTauri()) return "lattice diagnostics log (mock)\n— нет Tauri-бэкенда —";
  const c = await core();
  return c.invoke<string>("copy_log");
}

// --- События backend → stores ---------------------------------------------

let unlisteners: Array<() => void> = [];

export async function startEventBridge(): Promise<void> {
  if (!hasTauri()) return;
  const e = await events();
  unlisteners.push(
    await e.listen<ConnectionStatus>("status", (ev) => status.set(ev.payload)),
  );
  unlisteners.push(
    await e.listen<Peer[]>("peers", (ev) => peers.set(ev.payload)),
  );
  unlisteners.push(
    await e.listen<Diagnostics>("diagnostics", (ev) =>
      diagnostics.set(ev.payload),
    ),
  );
}

export function stopEventBridge(): void {
  unlisteners.forEach((u) => u());
  unlisteners = [];
}

// --- Mock для разработки в браузере ----------------------------------------

let mockTimer: ReturnType<typeof setTimeout> | undefined;

function mockConnect(args: ConnectArgs): void {
  status.set({ phase: "connecting", network: args.network });
  mockTimer = setTimeout(() => {
    status.set({
      phase: "connected",
      network: args.network,
      overlayIp: "10.66.0.1",
    });
    peers.set([
      { id: "p1", name: "Leonid-PC", overlayIp: "10.66.0.2", link: "p2p", pingMs: 18 },
      { id: "p2", name: "Misha-laptop", overlayIp: "10.66.0.3", link: "relay", pingMs: 64 },
      { id: "p3", name: "Danil-desktop", overlayIp: "10.66.0.4", link: "offline" },
    ]);
    diagnostics.set({ natType: "Full-cone", externalEndpoint: "85.x.x.x:51820" });
  }, 700);
}

function mockDisconnect(): void {
  if (mockTimer) clearTimeout(mockTimer);
  peers.set([]);
  status.set({ phase: "disconnected" });
}

// Восстановить сохранённые поля входа (чекбокс «Запомнить»).
export function restoreLogin(): void {
  if (typeof window === "undefined") return;
  try {
    const raw = window.localStorage.getItem("lattice.login");
    if (raw) {
      const v = JSON.parse(raw);
      loginForm.set({
        network: v.network ?? "",
        password: v.password ?? "",
        remember: true,
      });
    }
  } catch {
    /* игнорируем битый кеш */
  }
}

export function persistLogin(network: string, password: string, remember: boolean): void {
  if (typeof window === "undefined") return;
  if (remember) {
    window.localStorage.setItem("lattice.login", JSON.stringify({ network, password }));
  } else {
    window.localStorage.removeItem("lattice.login");
  }
}
