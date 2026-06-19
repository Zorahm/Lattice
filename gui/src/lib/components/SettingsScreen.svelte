<script lang="ts">
  import Icon from "./Icon.svelte";
  import Toggle from "./Toggle.svelte";
  import { t } from "../i18n";
  import { settings, diagnostics, screen, settingsReturn } from "../stores";
  import { saveSettings, getLog } from "../bridge";
  import type { Settings } from "../types";

  // Локальная редактируемая копия — применяется по «Сохранить».
  let draft = $state<Settings>(structuredClone($settings));
  let advancedOpen = $state(false);
  let savedFlash = $state(false);
  let copied = $state(false);

  function back() {
    screen.set($settingsReturn);
  }

  async function save() {
    await saveSettings(structuredClone(draft));
    savedFlash = true;
    setTimeout(() => (savedFlash = false), 1500);
  }

  async function copyLog() {
    const text = await getLog();
    try {
      await navigator.clipboard.writeText(text);
      copied = true;
      setTimeout(() => (copied = false), 1500);
    } catch {
      /* буфер недоступен — молча игнорируем */
    }
  }
</script>

<div class="wrap">
  <header>
    <button class="icon-btn" aria-label={t.back} onclick={back}>
      <Icon name="back" />
    </button>
    <span class="title">{t.settingsTitle}</span>
    <button class="primary save" onclick={save}>{savedFlash ? t.saved : t.save}</button>
  </header>

  <!-- Сеть -->
  <div class="sec-head"><Icon name="network" size={16} /><span>{t.secNetwork}</span></div>
  <div class="group">
    <div class="row">
      <span class="lbl">{t.fSubnet}</span>
      <input class="val mono" type="text" bind:value={draft.network.subnet} />
    </div>
    <div class="row">
      <span class="lbl">{t.fIpAssign}</span>
      <select class="val" bind:value={draft.network.ipAssign}>
        <option value="auto">{t.fIpAuto}</option>
        <option value="manual">{t.fIpManual}</option>
      </select>
    </div>
    {#if draft.network.ipAssign === "manual"}
      <div class="row">
        <span class="lbl">{t.fOverlayIp}</span>
        <input class="val mono" type="text" bind:value={draft.network.overlayIp} />
      </div>
    {/if}
    <div class="row last">
      <span class="lbl">{t.fMtu} <span class="muted">{t.fMtuHint}</span></span>
      <input class="val mono narrow" type="number" bind:value={draft.network.mtu} />
    </div>
  </div>

  <!-- Сервер координации -->
  <div class="sec-head"><Icon name="server" size={16} /><span>{t.secServer}</span></div>
  <div class="group">
    <div class="row">
      <span class="lbl">{t.fCoordination}</span>
      <input class="val mono" type="text" bind:value={draft.server.coordination} />
    </div>
    <div class="row last">
      <span class="lbl">{t.fStun}</span>
      <span class="val muted right">{t.fStunCount(draft.server.stun.length)}</span>
    </div>
  </div>

  <!-- Соединение -->
  <div class="sec-head"><Icon name="plug" size={16} /><span>{t.secConnection}</span></div>
  <div class="group">
    <div class="row">
      <span class="lbl">{t.fAllowRelay}</span>
      <Toggle bind:checked={draft.connection.allowRelay} label={t.fAllowRelay} />
    </div>
    <div class="row" class:last={!advancedOpen}>
      <span class="lbl">{t.fListenPort}</span>
      <input
        class="val mono narrow"
        type="number"
        placeholder="0"
        bind:value={draft.connection.listenPort}
      />
    </div>
    {#if advancedOpen}
      <div class="row last">
        <span class="lbl">{t.fKeepalive}</span>
        <div class="val right inline">
          <input class="mono narrow" type="number" bind:value={draft.connection.keepaliveSecs} />
          <span class="muted">{t.seconds}</span>
        </div>
      </div>
    {:else}
      <button class="advanced" onclick={() => (advancedOpen = true)}>{t.fAdvanced}</button>
    {/if}
  </div>

  <!-- Приложение -->
  <div class="sec-head"><Icon name="app" size={16} /><span>{t.secApp}</span></div>
  <div class="group">
    <div class="row">
      <span class="lbl">{t.fAutostart}</span>
      <Toggle bind:checked={draft.app.autostart} label={t.fAutostart} />
    </div>
    <div class="row">
      <span class="lbl">{t.fMinimizeToTray}</span>
      <Toggle bind:checked={draft.app.minimizeToTray} label={t.fMinimizeToTray} />
    </div>
    <div class="row last">
      <span class="lbl">{t.fLanguage}</span>
      <select class="val" bind:value={draft.app.language}>
        <option value="ru">{t.fLangRu}</option>
        <option value="en">English</option>
      </select>
    </div>
  </div>

  <!-- Диагностика -->
  <div class="sec-head"><Icon name="diagnostics" size={16} /><span>{t.secDiagnostics}</span></div>
  <div class="group">
    <div class="row">
      <span class="lbl muted">{t.fNatType}</span>
      <span class="val right muted">{$diagnostics.natType ?? t.unknown}</span>
    </div>
    <div class="row">
      <span class="lbl muted">{t.fExternalEndpoint}</span>
      <span class="val right muted mono">{$diagnostics.externalEndpoint ?? t.unknown}</span>
    </div>
    <div class="row last copy-row">
      <button class="copy-btn" onclick={copyLog}>
        <Icon name="copy" size={15} />
        {copied ? t.copied : t.copyLog}
      </button>
    </div>
  </div>
</div>

<style>
  .wrap {
    background: var(--color-background-secondary);
    border-radius: var(--border-radius-lg);
    padding: 1.5rem;
    max-width: 460px;
    margin: 0 auto;
  }
  header {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-bottom: 1.5rem;
  }
  header .title {
    font-weight: 500;
    font-size: 16px;
    flex: 1;
  }
  .save {
    height: 30px;
    padding: 0 14px;
    font-size: 13px;
  }
  .icon-btn {
    background: none;
    border: none;
    padding: 6px;
    border-radius: var(--border-radius-sm);
    color: var(--color-text-secondary);
    display: inline-flex;
  }
  .icon-btn:hover {
    color: var(--color-text-primary);
    background: var(--color-background-tertiary);
  }
  .sec-head {
    display: flex;
    align-items: center;
    gap: 7px;
    margin-bottom: 10px;
    color: var(--color-text-tertiary);
  }
  .sec-head span {
    font-size: 12px;
    font-weight: 500;
    color: var(--color-text-secondary);
  }
  .group {
    background: var(--color-background-primary);
    border: 1px solid var(--color-border-tertiary);
    border-radius: var(--border-radius-md);
    padding: 0 14px;
    margin-bottom: 1.25rem;
  }
  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 8px 0;
    border-bottom: 1px solid var(--color-border-tertiary);
    min-height: 42px;
  }
  .row.last {
    border-bottom: none;
  }
  .lbl {
    font-size: 14px;
    /* лейбл сжимается первым — иначе длинная подпись (MTU…) выталкивает
       значение за край и появляется горизонтальный скролл */
    flex: 1 1 auto;
    min-width: 0;
  }
  .muted {
    color: var(--color-text-tertiary);
    font-size: 11px;
  }
  .lbl.muted {
    font-size: 14px;
    color: var(--color-text-secondary);
  }
  .val {
    font-size: 13px;
    color: var(--color-text-secondary);
    flex-shrink: 0;
    max-width: 62%;
  }
  .mono {
    font-family: var(--font-mono);
  }
  .right {
    text-align: right;
  }
  /* поля ввода справа: компактно, как значения в макете */
  input.val,
  select.val {
    width: auto;
    max-width: 60%;
    height: 30px;
    text-align: right;
    background: transparent;
    border-color: transparent;
  }
  input.val:hover,
  select.val:hover,
  input.val:focus,
  select.val:focus {
    background: var(--color-background-secondary);
    border-color: var(--color-border-primary);
  }
  select.val {
    text-align: left;
  }
  .narrow {
    width: 90px;
    flex-shrink: 0;
    height: 30px;
    text-align: right;
    background: transparent;
    border-color: transparent;
  }
  .narrow:hover,
  .narrow:focus {
    background: var(--color-background-secondary);
    border-color: var(--color-border-primary);
  }
  .inline {
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }
  .advanced {
    background: none;
    border: none;
    color: var(--color-text-tertiary);
    font-size: 12px;
    padding: 12px 0;
    width: 100%;
    text-align: right;
  }
  .advanced:hover {
    color: var(--color-text-secondary);
    background: none;
  }
  .copy-row {
    padding: 12px 0 10px;
  }
  .copy-btn {
    width: 100%;
    height: 34px;
    font-size: 13px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 6px;
  }
</style>
