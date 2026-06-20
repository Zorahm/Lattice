<script lang="ts">
  import type { Peer } from "../types";
  import { linkTitle, t } from "../i18n";

  let {
    peer,
    onmenu,
  }: { peer: Peer; onmenu: (peer: Peer, x: number, y: number) => void } = $props();

  const color: Record<Peer["link"], string> = {
    p2p: "var(--color-link-direct)",
    relay: "var(--color-link-relay)",
    offline: "var(--color-link-offline)",
  };

  // peer.name = hostname-pid; показываем чистый hostname.
  const cleanName = $derived(peer.name.replace(/-\d+$/, ""));

  function onContext(e: MouseEvent) {
    e.preventDefault();
    onmenu(peer, e.clientX, e.clientY);
  }

  function onKebab(e: MouseEvent) {
    e.stopPropagation();
    const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
    onmenu(peer, r.right, r.bottom + 4);
  }
</script>

<div
  class="peer"
  class:offline={peer.link === "offline"}
  role="button"
  tabindex="0"
  oncontextmenu={onContext}
>
  <span class="dot" style={`background:${color[peer.link]}`} title={linkTitle(peer.link)}></span>
  <div class="meta">
    <div class="name">{cleanName}</div>
    <div class="ip">{peer.overlayIp}</div>
  </div>
  <span class="ping">
    {#if peer.link === "offline"}
      {t.offline}
    {:else if peer.pingMs != null}
      {peer.pingMs} ms
    {/if}
  </span>
  <button class="kebab" aria-label={t.ctxProperties} onclick={onKebab}>
    <svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <circle cx="12" cy="5" r="1.6" />
      <circle cx="12" cy="12" r="1.6" />
      <circle cx="12" cy="19" r="1.6" />
    </svg>
  </button>
</div>

<style>
  .peer {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 8px 6px 8px 10px;
    border-radius: var(--border-radius-md);
    transition: background 0.12s ease;
  }
  .peer:hover {
    background: var(--color-background-hover);
  }
  .peer.offline {
    opacity: 0.5;
  }
  .dot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .meta {
    flex: 1;
    min-width: 0;
  }
  .name {
    font-size: 14px;
    font-weight: 500;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .ip {
    font-size: 12px;
    color: var(--color-text-tertiary);
    font-family: var(--font-mono);
  }
  .ping {
    font-size: 12px;
    color: var(--color-text-tertiary);
    white-space: nowrap;
  }
  .kebab {
    flex-shrink: 0;
    width: 26px;
    height: 26px;
    padding: 0;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    background: none;
    border: none;
    border-radius: var(--border-radius-sm);
    color: var(--color-text-tertiary);
    opacity: 0;
    transition: opacity 0.12s ease, color 0.12s ease, background 0.12s ease;
  }
  .peer:hover .kebab {
    opacity: 1;
  }
  .kebab:hover {
    color: var(--color-text-primary);
    background: var(--color-background-tertiary);
  }
</style>
