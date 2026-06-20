<script lang="ts">
  import Icon from "./Icon.svelte";
  import PeerRow from "./PeerRow.svelte";
  import ContextMenu, { type MenuItem } from "./ContextMenu.svelte";
  import PeerProperties from "./PeerProperties.svelte";
  import RoomDialog from "./RoomDialog.svelte";
  import type { Peer, Room } from "../types";
  import { t, errorMessage } from "../i18n";
  import {
    status,
    peers,
    rooms,
    activeRoomId,
    selfName,
    addRoom,
    removeRoom,
    persistSelfName,
    openSettings,
  } from "../stores";
  import { connect, disconnect, installTapDriver } from "../bridge";

  // --- Своё имя --------------------------------------------------------------
  // Показываем пользовательское имя; пока не задано — hostname с backend.
  const selfDisplay = $derived($selfName.trim() || $status.selfName || "—");
  let editingName = $state(false);
  let nameDraft = $state("");

  function startEditName() {
    nameDraft = $selfName.trim() || $status.selfName || "";
    editingName = true;
  }
  async function saveName() {
    editingName = false;
    const next = nameDraft.trim();
    persistSelfName(next);
    // Имя — часть peer-id, поэтому применяется переподключением активной комнаты.
    const room = activeRoom();
    if (room && $status.phase !== "disconnected") {
      await connect({ network: room.name, password: room.password, displayName: next });
    }
  }

  // --- Комнаты ---------------------------------------------------------------
  function activeRoom(): Room | undefined {
    return $rooms.find((r) => r.id === $activeRoomId);
  }

  async function connectRoom(room: Room) {
    activeRoomId.set(room.id);
    await connect({ network: room.name, password: room.password, displayName: $selfName.trim() });
  }

  async function disconnectActive() {
    await disconnect();
    activeRoomId.set(null);
  }

  async function leaveRoom(room: Room) {
    if (room.id === $activeRoomId) await disconnectActive();
    removeRoom(room.id);
  }

  // Состояние строки комнаты: активная отражает фазу подключения, прочие — офлайн.
  function roomPhase(room: Room): "connected" | "connecting" | "error" | "idle" {
    if (room.id !== $activeRoomId) return "idle";
    if ($status.phase === "connected") return "connected";
    if ($status.phase === "connecting" || $status.phase === "reconnecting") return "connecting";
    if ($status.phase === "error") return "error";
    return "idle";
  }
  const phaseColor: Record<string, string> = {
    connected: "var(--color-link-direct)",
    connecting: "var(--color-link-relay)",
    error: "var(--color-danger)",
    idle: "var(--color-link-offline)",
  };
  function phaseLabel(p: string): string {
    if (p === "connected") return t.roomConnected;
    if (p === "connecting") return t.roomConnecting;
    if (p === "error") return t.roomDisconnected;
    return t.roomDisconnected;
  }

  // --- Ошибки подключения / установка драйвера -------------------------------
  const errored = $derived($status.phase === "error" && $activeRoomId != null);
  const noDriver = $derived(errored && $status.error?.kind === "no_tap_driver");
  let installing = $state(false);

  async function installDriver() {
    installing = true;
    try {
      await installTapDriver();
      const room = activeRoom();
      if (room) await connectRoom(room); // драйвер поставлен — пробуем снова
    } catch {
      /* ошибка вернётся событием status */
    } finally {
      installing = false;
    }
  }

  // --- Диалоги создать/присоединиться ----------------------------------------
  let dialog = $state<"create" | "join" | null>(null);

  async function onDialogSubmit(name: string, password: string) {
    const room = addRoom(name, password);
    dialog = null;
    await connectRoom(room);
  }

  // --- Контекстное меню пира (активная комната) ------------------------------
  let menu = $state<{ peer: Peer; x: number; y: number } | null>(null);
  let propsPeer = $state<Peer | null>(null);
  let toast = $state("");
  let toastTimer: ReturnType<typeof setTimeout> | undefined;

  function openMenu(peer: Peer, x: number, y: number) {
    menu = { peer, x, y };
  }
  async function copy(text: string) {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      /* webview без clipboard — игнорируем */
    }
    flash(t.copiedShort);
  }
  function flash(msg: string) {
    toast = msg;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toast = ""), 1400);
  }
  const menuItems = (peer: Peer): MenuItem[] => [
    { label: t.ctxCopyIp, onSelect: () => copy(peer.overlayIp) },
    { label: t.ctxCopyName, onSelect: () => copy(peer.name.replace(/-\d+$/, "")) },
    { label: t.ctxProperties, onSelect: () => (propsPeer = peer) },
  ];

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

  const selfPhase = $derived(roomPhase(activeRoom() ?? ({ id: "" } as Room)));
</script>

<div class="screen">
  <header class="topbar">
    <div class="brand">
      <Icon name="logo" size={18} />
      <span>{t.appName}</span>
    </div>
    <button class="icon-btn" aria-label={t.settings} onclick={() => openSettings("rooms")}>
      <Icon name="settings" size={18} />
    </button>
  </header>

  <!-- Карточка себя: имя редактируется по клику -->
  <div class="self">
    <span
      class="self-ring"
      class:online={selfPhase === "connected"}
      class:reconnecting={selfPhase === "connecting"}
    ></span>
    <div class="self-info">
      {#if editingName}
        <!-- svelte-ignore a11y_autofocus -->
        <input
          class="name-edit"
          autofocus
          bind:value={nameDraft}
          onblur={saveName}
          onkeydown={(e) => {
            if (e.key === "Enter") saveName();
            else if (e.key === "Escape") editingName = false;
          }}
        />
        <div class="self-hint">{t.editNameHint}</div>
      {:else}
        <button class="self-name" onclick={startEditName} title={t.editName}>
          {selfDisplay}
          <span class="edit-pencil">✎</span>
        </button>
        <div class="self-ip">{$status.overlayIp ?? ""}</div>
      {/if}
    </div>
  </div>

  {#if errored}
    {@const m = errorMessage($status.error)}
    <div class="error" role="alert">
      <strong>{m.title}.</strong>
      {m.action}
      {#if noDriver}
        <button class="install" disabled={installing} onclick={installDriver}>
          {installing ? "Установка…" : "Установить?"}
        </button>
      {/if}
    </div>
  {/if}

  {#if $rooms.length === 0}
    <!-- Пусто: предложить создать или присоединиться -->
    <section class="chooser">
      <h2>{t.noRoomsTitle}</h2>
      <p>{t.noRoomsHint}</p>
      <div class="chooser-actions">
        <button class="primary" onclick={() => (dialog = "create")}>{t.createRoom}</button>
        <button class="ghost" onclick={() => (dialog = "join")}>{t.joinRoom}</button>
      </div>
    </section>
  {:else}
    <section class="rooms">
      <div class="rooms-head">
        <span class="rooms-title">{t.myRooms}</span>
        <button class="add" aria-label={t.createRoom} onclick={() => (dialog = "create")}>+</button>
      </div>

      {#each $rooms as room (room.id)}
        {@const phase = roomPhase(room)}
        {@const active = room.id === $activeRoomId}
        <div class="room" class:active>
          <div class="room-head">
            <span class="dot" style={`background:${phaseColor[phase]}`}></span>
            <div class="room-meta">
              <div class="room-name">{room.name}</div>
              <div class="room-state">
                {#if active && phase === "connected"}
                  {t.peersCount($peers.length)}
                {:else}
                  {phaseLabel(phase)}
                {/if}
              </div>
            </div>
            {#if active}
              <button class="room-btn ghost" onclick={disconnectActive}>{t.disconnect}</button>
            {:else}
              <button class="room-btn" onclick={() => connectRoom(room)}>{t.roomConnect}</button>
            {/if}
            <button class="leave" aria-label={t.roomLeave} onclick={() => leaveRoom(room)}>×</button>
          </div>

          {#if active && (phase === "connected" || phase === "connecting")}
            <div class="list">
              {#if $peers.length === 0}
                <p class="empty">{t.emptyPeers}</p>
              {:else}
                {#each $peers as peer (peer.id)}
                  <PeerRow {peer} onmenu={openMenu} />
                {/each}
              {/if}
            </div>
          {/if}
        </div>
      {/each}

      <div class="rooms-actions">
        <button class="primary" onclick={() => (dialog = "create")}>{t.createRoom}</button>
        <button class="ghost" onclick={() => (dialog = "join")}>{t.joinRoom}</button>
      </div>
    </section>

    <div class="legend">
      {#each legend as item}
        <span class="legend-item">
          <span class="dot" style={`background:${legendColor[item.link]}`}></span>
          {item.label}
        </span>
      {/each}
    </div>
  {/if}
</div>

{#if dialog}
  <RoomDialog mode={dialog} onClose={() => (dialog = null)} onSubmit={onDialogSubmit} />
{/if}

{#if menu}
  <ContextMenu x={menu.x} y={menu.y} items={menuItems(menu.peer)} onClose={() => (menu = null)} />
{/if}

{#if propsPeer}
  <PeerProperties peer={propsPeer} onClose={() => (propsPeer = null)} />
{/if}

{#if toast}
  <div class="toast">{toast}</div>
{/if}

<style>
  .screen {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .topbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  .brand {
    display: flex;
    align-items: center;
    gap: 8px;
    font-weight: 600;
    font-size: 15px;
    letter-spacing: 0.2px;
    color: var(--color-text-primary);
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

  /* --- карточка себя --- */
  .self {
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 16px;
    background: var(--color-background-secondary);
    border: 1px solid var(--color-border-secondary);
    border-radius: var(--border-radius-lg);
  }
  .self-ring {
    width: 14px;
    height: 14px;
    border-radius: 50%;
    flex-shrink: 0;
    background: var(--color-link-offline);
  }
  .self-ring.online {
    background: var(--color-text-success);
    box-shadow: 0 0 0 4px color-mix(in srgb, var(--color-text-success) 22%, transparent);
  }
  .self-ring.reconnecting {
    background: var(--color-link-relay);
    box-shadow: 0 0 0 4px color-mix(in srgb, var(--color-link-relay) 22%, transparent);
    animation: pulse 1.1s ease-in-out infinite;
  }
  @keyframes pulse {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.4;
    }
  }
  .self-info {
    flex: 1;
    min-width: 0;
  }
  .self-name {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    background: none;
    border: none;
    padding: 0;
    font-size: 16px;
    font-weight: 600;
    color: var(--color-text-primary);
    max-width: 100%;
  }
  .self-name:hover .edit-pencil {
    opacity: 1;
  }
  .edit-pencil {
    font-size: 12px;
    color: var(--color-text-tertiary);
    opacity: 0;
    transition: opacity 0.12s ease;
  }
  .name-edit {
    width: 100%;
    height: 30px;
    font-size: 15px;
    font-weight: 600;
  }
  .self-ip {
    font-size: 13px;
    color: var(--color-text-tertiary);
    font-family: var(--font-mono);
  }
  .self-hint {
    font-size: 11px;
    color: var(--color-text-tertiary);
    margin-top: 4px;
  }

  /* --- пустое состояние --- */
  .chooser {
    background: var(--color-background-secondary);
    border: 1px solid var(--color-border-secondary);
    border-radius: var(--border-radius-lg);
    padding: 1.75rem 1.5rem;
    text-align: center;
  }
  .chooser h2 {
    margin: 0 0 8px;
    font-size: 16px;
    font-weight: 600;
  }
  .chooser p {
    margin: 0 0 1.25rem;
    font-size: 13px;
    line-height: 1.5;
    color: var(--color-text-tertiary);
  }
  .chooser-actions {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .chooser-actions button {
    height: 40px;
    font-size: 14px;
  }

  /* --- список комнат --- */
  .rooms {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .rooms-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0 2px;
  }
  .rooms-title {
    font-size: 13px;
    font-weight: 600;
    color: var(--color-text-secondary);
  }
  .add {
    width: 26px;
    height: 26px;
    padding: 0;
    font-size: 18px;
    line-height: 1;
    border-radius: var(--border-radius-sm);
    background: var(--color-background-tertiary);
    border: 1px solid var(--color-border-primary);
    color: var(--color-text-secondary);
  }
  .add:hover {
    color: var(--color-text-primary);
  }
  .room {
    background: var(--color-background-secondary);
    border: 1px solid var(--color-border-secondary);
    border-radius: var(--border-radius-lg);
    padding: 8px;
  }
  .room.active {
    border-color: color-mix(in srgb, var(--color-text-success) 45%, var(--color-border-secondary));
  }
  .room-head {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 6px 4px 6px 8px;
  }
  .room-head .dot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .room-meta {
    flex: 1;
    min-width: 0;
  }
  .room-name {
    font-weight: 600;
    font-size: 14px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .room-state {
    font-size: 12px;
    color: var(--color-text-tertiary);
  }
  .room-btn {
    flex-shrink: 0;
    height: 30px;
    padding: 0 12px;
    font-size: 13px;
  }
  .room-btn.ghost {
    background: var(--color-background-tertiary);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-border-primary);
  }
  .room-btn.ghost:hover {
    color: var(--color-text-primary);
  }
  .leave {
    flex-shrink: 0;
    width: 26px;
    height: 26px;
    padding: 0;
    font-size: 16px;
    line-height: 1;
    background: none;
    border: none;
    border-radius: var(--border-radius-sm);
    color: var(--color-text-tertiary);
  }
  .leave:hover {
    color: var(--color-danger);
    background: var(--color-background-tertiary);
  }
  .list {
    display: flex;
    flex-direction: column;
    gap: 1px;
    padding: 4px 0 2px;
    border-top: 1px solid var(--color-border-tertiary);
    margin-top: 4px;
  }
  .empty {
    text-align: center;
    max-width: 240px;
    margin: 0 auto;
    font-size: 13px;
    color: var(--color-text-tertiary);
    line-height: 1.5;
    padding: 1rem;
  }
  .rooms-actions {
    display: flex;
    gap: 10px;
    margin-top: 4px;
  }
  .rooms-actions button {
    flex: 1;
    height: 38px;
    font-size: 14px;
  }
  .ghost {
    background: var(--color-background-tertiary);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-border-primary);
  }
  .ghost:hover {
    color: var(--color-text-primary);
  }

  /* --- ошибка --- */
  .error {
    font-size: 13px;
    line-height: 1.5;
    color: var(--color-text-primary);
    background: color-mix(in srgb, var(--color-danger) 14%, transparent);
    border: 1px solid color-mix(in srgb, var(--color-danger) 40%, transparent);
    border-radius: var(--border-radius-md);
    padding: 10px 12px;
  }
  .install {
    display: block;
    margin-top: 8px;
    height: 32px;
    padding: 0 14px;
    font-size: 13px;
  }

  /* --- легенда --- */
  .legend {
    display: flex;
    align-items: center;
    gap: 14px;
    font-size: 11px;
    color: var(--color-text-tertiary);
    padding: 0 2px;
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

  .toast {
    position: fixed;
    bottom: 18px;
    left: 50%;
    transform: translateX(-50%);
    z-index: 60;
    padding: 8px 16px;
    font-size: 13px;
    color: var(--color-text-primary);
    background: var(--color-background-tertiary);
    border: 1px solid var(--color-border-primary);
    border-radius: 999px;
    box-shadow: var(--shadow-pop);
    animation: fade 0.12s ease-out;
  }
  @keyframes fade {
    from {
      opacity: 0;
    }
  }
</style>
