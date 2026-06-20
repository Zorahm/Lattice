<script lang="ts">
  import Icon from "./Icon.svelte";
  import { t } from "../i18n";

  let {
    mode,
    onClose,
    onSubmit,
  }: {
    mode: "create" | "join";
    onClose: () => void;
    onSubmit: (name: string, password: string) => void;
  } = $props();

  let name = $state("");
  let password = $state("");
  let showPassword = $state(false);

  const title = $derived(mode === "create" ? t.createRoom : t.joinRoom);
  const submitLabel = $derived(mode === "create" ? t.create : t.join);
  const canSubmit = $derived(name.trim().length > 0 && password.length > 0);

  function submit() {
    if (!canSubmit) return;
    onSubmit(name.trim(), password);
  }

  // Для создания — предложить случайный пароль (можно изменить).
  function generate() {
    const a = new Uint8Array(9);
    crypto.getRandomValues(a);
    password = btoa(String.fromCharCode(...a)).replace(/[+/=]/g, "").slice(0, 12);
    showPassword = true;
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") onClose();
  }
</script>

<svelte:window onkeydown={onKey} />

<div
  class="overlay"
  role="button"
  tabindex="-1"
  aria-label={t.cancel}
  onclick={onClose}
  onkeydown={(e) => e.key === "Enter" && onClose()}
>
  <div
    class="dialog"
    role="dialog"
    aria-modal="true"
    tabindex="-1"
    onclick={(e) => e.stopPropagation()}
    onkeydown={() => {}}
  >
    <h2>{title}</h2>

    <div class="field">
      <label for="rname">{t.roomNameLabel}</label>
      <input
        id="rname"
        type="text"
        autocomplete="off"
        placeholder={t.networkPlaceholder}
        bind:value={name}
        onkeydown={(e) => e.key === "Enter" && submit()}
      />
    </div>

    <div class="field">
      <label for="rpwd">{t.passwordLabel}</label>
      <div class="pwd-wrap">
        <input
          id="rpwd"
          type={showPassword ? "text" : "password"}
          autocomplete="off"
          bind:value={password}
          onkeydown={(e) => e.key === "Enter" && submit()}
        />
        <button
          type="button"
          class="icon-btn eye"
          aria-label={showPassword ? t.hidePassword : t.showPassword}
          onclick={() => (showPassword = !showPassword)}
        >
          <Icon name={showPassword ? "eye-off" : "eye"} />
        </button>
      </div>
      {#if mode === "create"}
        <button type="button" class="gen" onclick={generate}>🎲 случайный пароль</button>
      {/if}
    </div>

    <p class="hint">{t.loginHint}</p>

    <div class="actions">
      <button class="ghost" onclick={onClose}>{t.cancel}</button>
      <button class="primary" disabled={!canSubmit} onclick={submit}>{submitLabel}</button>
    </div>
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
    max-width: 340px;
    background: var(--color-background-secondary);
    border: 1px solid var(--color-border-primary);
    border-radius: var(--border-radius-lg);
    padding: 1.25rem;
    box-shadow: var(--shadow-pop);
  }
  h2 {
    margin: 0 0 1.1rem;
    font-size: 16px;
    font-weight: 600;
  }
  .field {
    margin-bottom: 14px;
  }
  .field label {
    display: block;
    font-size: 13px;
    color: var(--color-text-secondary);
    margin-bottom: 5px;
  }
  .pwd-wrap {
    position: relative;
  }
  .pwd-wrap input {
    padding-right: 40px;
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
  .eye {
    position: absolute;
    right: 5px;
    top: 50%;
    transform: translateY(-50%);
  }
  .gen {
    margin-top: 6px;
    padding: 0;
    background: none;
    border: none;
    font-size: 12px;
    color: var(--color-text-tertiary);
  }
  .gen:hover {
    color: var(--color-text-primary);
  }
  .hint {
    font-size: 12px;
    color: var(--color-text-tertiary);
    margin: 0 0 1.1rem;
    line-height: 1.5;
  }
  .actions {
    display: flex;
    gap: 10px;
  }
  .actions button {
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
</style>
