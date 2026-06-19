<script lang="ts">
  import Icon from "./Icon.svelte";
  import PeerRow from "./PeerRow.svelte";
  import { t } from "../i18n";
  import { status, peers, loginForm, openSettings } from "../stores";
  import { disconnect } from "../bridge";

  const reconnecting = $derived($status.phase === "reconnecting");
  const network = $derived($status.network ?? $loginForm.network);
  const overlayIp = $derived($status.overlayIp ?? "");

  const legend: { link: "p2p" | "relay" | "offline"; label: string }[] = [
    { link: "p2p", label: t.legendDirect },
    { link: "relay", label: t.legendRelay },
    { link: "offline", label: t.legendOffline },
  ];
  const legendColor: Record<string, string> = {
    p2p: "var(--color-link-direct)",
    relay: "var(--color-link-relay)",
    offline: "var(--color-link-offline)",
  };
</script>

<div class="card">
  <header>
    <div class="head-left">
      <div class="title">
        <span
          class="status-dot"
          class:reconnecting
          style={`background:${reconnecting ? "var(--color-link-relay)" : "var(--color-text-success)"}`}
        ></span>
        <span class="net-name">{network}</span>
      </div>
      <span class="sub">
        {#if reconnecting}
          {t.reconnecting}
        {:else}
          {t.you} — {overlayIp}
        {/if}
      </span>
    </div>
    <button class="icon-btn" aria-label={t.settings} onclick={() => openSettings("connected")}>
      <Icon name="settings" />
    </button>
  </header>

  <div class="list">
    {#if $peers.length === 0}
      <p class="empty">{t.emptyPeers}</p>
    {:else}
      {#each $peers as peer (peer.id)}
        <PeerRow {peer} />
      {/each}
    {/if}
  </div>

  <div class="legend">
    {#each legend as item}
      <span class="legend-item">
        <span class="dot" style={`background:${legendColor[item.link]}`}></span>
        {item.label}
      </span>
    {/each}
  </div>

  <button class="disconnect" onclick={disconnect}>{t.disconnect}</button>
</div>

<style>
  .card {
    background: var(--color-background-secondary);
    border-radius: var(--border-radius-lg);
    padding: 1.5rem;
    display: flex;
    flex-direction: column;
    min-height: 420px;
  }
  header {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    margin-bottom: 1rem;
  }
  .title {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .status-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
  }
  .status-dot.reconnecting {
    animation: pulse 1.1s ease-in-out infinite;
  }
  @keyframes pulse {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.35;
    }
  }
  .net-name {
    font-weight: 500;
    font-size: 15px;
  }
  .sub {
    display: block;
    font-size: 12px;
    color: var(--color-text-tertiary);
    margin-left: 16px;
    font-family: var(--font-mono);
  }
  .icon-btn {
    background: none;
    border: none;
    padding: 6px;
    border-radius: var(--border-radius-sm);
    color: var(--color-text-tertiary);
    display: inline-flex;
  }
  .icon-btn:hover {
    color: var(--color-text-primary);
    background: var(--color-background-tertiary);
  }
  .list {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .empty {
    margin: auto;
    text-align: center;
    max-width: 240px;
    font-size: 13px;
    color: var(--color-text-tertiary);
    line-height: 1.5;
  }
  .legend {
    display: flex;
    align-items: center;
    gap: 14px;
    margin-top: 12px;
    padding-top: 10px;
    border-top: 1px solid var(--color-border-tertiary);
    font-size: 11px;
    color: var(--color-text-tertiary);
  }
  .legend-item {
    display: flex;
    align-items: center;
    gap: 5px;
  }
  .legend .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
  }
  .disconnect {
    margin-top: 12px;
    height: 36px;
    font-size: 14px;
  }
</style>
