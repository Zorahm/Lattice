<script lang="ts">
  import type { Peer } from "../types";
  import { linkTitle, t } from "../i18n";

  let { peer, onClose }: { peer: Peer; onClose: () => void } = $props();

  const linkColor: Record<Peer["link"], string> = {
    p2p: "var(--color-link-direct)",
    relay: "var(--color-link-relay)",
    offline: "var(--color-link-offline)",
  };

  // Имя пира приходит как hostname-pid; на экране показываем чистый hostname.
  const cleanName = $derived(peer.name.replace(/-\d+$/, ""));

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") onClose();
  }
</script>

<svelte:window onkeydown={onKey} />

<div
  class="overlay"
  role="button"
  tabindex="-1"
  aria-label={t.close}
  onclick={onClose}
  onkeydown={(e) => e.key === "Enter" && onClose()}
>
  <!-- stopPropagation: клик внутри карточки не закрывает оверлей -->
  <div class="dialog" role="dialog" aria-modal="true" onclick={(e) => e.stopPropagation()} onkeydown={() => {}} tabindex="-1">
    <div class="head">
      <span class="dot" style={`background:${linkColor[peer.link]}`}></span>
      <h2>{cleanName}</h2>
    </div>

    <dl class="rows">
      <div class="row">
        <dt>{t.propName}</dt>
        <dd>{cleanName}</dd>
      </div>
      <div class="row">
        <dt>{t.propOverlayIp}</dt>
        <dd class="mono">{peer.overlayIp}</dd>
      </div>
      <div class="row">
        <dt>{t.propLink}</dt>
        <dd>{linkTitle(peer.link)}</dd>
      </div>
      <div class="row">
        <dt>{t.propPing}</dt>
        <dd>{peer.link !== "offline" && peer.pingMs != null ? `${peer.pingMs} ms` : t.unknown}</dd>
      </div>
    </dl>

    <button class="primary close" onclick={onClose}>{t.close}</button>
  </div>
</div>

<style>
  .overlay {
    position: fixed;
    inset: 0;
    z-index: 40;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 1rem;
    background: rgba(0, 0, 0, 0.5);
    animation: fade 0.12s ease-out;
  }
  @keyframes fade {
    from {
      opacity: 0;
    }
  }
  .dialog {
    width: 100%;
    max-width: 320px;
    background: var(--color-background-secondary);
    border: 1px solid var(--color-border-primary);
    border-radius: var(--border-radius-lg);
    padding: 1.25rem;
    box-shadow: var(--shadow-pop);
  }
  .head {
    display: flex;
    align-items: center;
    gap: 9px;
    margin-bottom: 1rem;
  }
  .head .dot {
    width: 10px;
    height: 10px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  h2 {
    margin: 0;
    font-size: 16px;
    font-weight: 600;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .rows {
    margin: 0 0 1.25rem;
    display: flex;
    flex-direction: column;
  }
  .row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 12px;
    padding: 8px 0;
    border-top: 1px solid var(--color-border-tertiary);
    font-size: 13px;
  }
  .row:first-child {
    border-top: none;
  }
  dt {
    color: var(--color-text-tertiary);
  }
  dd {
    margin: 0;
    color: var(--color-text-primary);
    text-align: right;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .mono {
    font-family: var(--font-mono);
  }
  .close {
    width: 100%;
    height: 36px;
  }
</style>
