<script module lang="ts">
  export interface MenuItem {
    label: string;
    onSelect: () => void;
    danger?: boolean;
  }
</script>

<script lang="ts">
  // Лёгкое контекстное меню у курсора. Закрывается по клику вне, Escape, скроллу
  // и потере фокуса окна. Позиция клампится в видимую область.
  import { onMount } from "svelte";

  let {
    x,
    y,
    items,
    onClose,
  }: { x: number; y: number; items: MenuItem[]; onClose: () => void } = $props();

  let el: HTMLDivElement | undefined = $state();
  // Подправляем позицию после рендера, чтобы меню не вылезало за край.
  let pos = $state({ left: x, top: y });

  onMount(() => {
    if (el) {
      const r = el.getBoundingClientRect();
      const pad = 8;
      const left = Math.min(x, window.innerWidth - r.width - pad);
      const top = Math.min(y, window.innerHeight - r.height - pad);
      pos = { left: Math.max(pad, left), top: Math.max(pad, top) };
    }
    const close = () => onClose();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    // capture: ловим клик раньше, чем он дойдёт до элементов под меню.
    window.addEventListener("pointerdown", onPointer, true);
    window.addEventListener("keydown", onKey);
    window.addEventListener("blur", close);
    window.addEventListener("resize", close);
    return () => {
      window.removeEventListener("pointerdown", onPointer, true);
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("blur", close);
      window.removeEventListener("resize", close);
    };
  });

  function onPointer(e: PointerEvent) {
    if (el && !el.contains(e.target as Node)) onClose();
  }

  function pick(item: MenuItem) {
    item.onSelect();
    onClose();
  }
</script>

<div
  bind:this={el}
  class="menu"
  style={`left:${pos.left}px; top:${pos.top}px`}
  role="menu"
  tabindex="-1"
>
  {#each items as item}
    <button class="item" class:danger={item.danger} role="menuitem" onclick={() => pick(item)}>
      {item.label}
    </button>
  {/each}
</div>

<style>
  .menu {
    position: fixed;
    z-index: 50;
    min-width: 184px;
    padding: 5px;
    background: var(--color-background-secondary);
    border: 1px solid var(--color-border-primary);
    border-radius: var(--border-radius-md);
    box-shadow: var(--shadow-pop);
    display: flex;
    flex-direction: column;
    gap: 1px;
    animation: pop 0.09s ease-out;
  }
  @keyframes pop {
    from {
      opacity: 0;
      transform: scale(0.97);
    }
  }
  .item {
    width: 100%;
    text-align: left;
    padding: 7px 10px;
    font-size: 13px;
    background: none;
    border: none;
    border-radius: var(--border-radius-sm);
    color: var(--color-text-secondary);
  }
  .item:hover:not(:disabled) {
    background: var(--color-background-hover);
    color: var(--color-text-primary);
  }
  .item.danger {
    color: var(--color-danger);
  }
</style>
