// Тёмная/светлая тема. По умолчанию следуем системе, но даём явное
// переключение. Применяется через data-theme на <html>.

import { writable } from "svelte/store";
import type { Theme } from "./types";

const STORAGE_KEY = "lattice.theme";

function initial(): Theme {
  if (typeof window === "undefined") return "dark";
  const saved = window.localStorage.getItem(STORAGE_KEY);
  if (saved === "dark" || saved === "light") return saved;
  const prefersLight =
    window.matchMedia &&
    window.matchMedia("(prefers-color-scheme: light)").matches;
  return prefersLight ? "light" : "dark";
}

export const theme = writable<Theme>("dark");

export function applyTheme(value: Theme): void {
  if (typeof document !== "undefined") {
    document.documentElement.setAttribute("data-theme", value);
  }
}

/** Вызвать один раз при старте приложения. */
export function initTheme(): void {
  const value = initial();
  theme.set(value);
  applyTheme(value);
  theme.subscribe((v) => {
    applyTheme(v);
    if (typeof window !== "undefined") {
      window.localStorage.setItem(STORAGE_KEY, v);
    }
  });
}

export function toggleTheme(): void {
  theme.update((v) => (v === "dark" ? "light" : "dark"));
}
