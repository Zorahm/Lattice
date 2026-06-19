<script lang="ts">
  import type { Peer } from "../types";
  import { linkTitle, t } from "../i18n";

  let { peer }: { peer: Peer } = $props();

  const color: Record<Peer["link"], string> = {
    p2p: "var(--color-link-direct)",
    relay: "var(--color-link-relay)",
    offline: "var(--color-link-offline)",
  };
</script>

<div class="peer" class:offline={peer.link === "offline"}>
  <!-- только цвет + tooltip; терминов NAT/relay на экране нет -->
  <span class="dot" style={`background:${color[peer.link]}`} title={linkTitle(peer.link)}></span>
  <div class="meta">
    <div class="name">{peer.name}</div>
    <div class="ip">{peer.overlayIp}</div>
  </div>
  <span class="ping">
    {#if peer.link === "offline"}
      {t.offline}
    {:else if peer.pingMs != null}
      {peer.pingMs} ms
    {/if}
  </span>
</div>

<style>
  .peer {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 9px 10px;
    background: var(--color-background-primary);
    border: 1px solid var(--color-border-tertiary);
    border-radius: var(--border-radius-md);
  }
  .peer.offline {
    opacity: 0.55;
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
</style>
