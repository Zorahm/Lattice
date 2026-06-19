<script lang="ts">
  import Icon from "./Icon.svelte";
  import { t, errorMessage } from "../i18n";
  import { status, loginForm, openSettings } from "../stores";
  import { connect, persistLogin } from "../bridge";

  let showPassword = $state(false);

  const busy = $derived($status.phase === "connecting");
  const errored = $derived($status.phase === "error");
  const canConnect = $derived(
    $loginForm.network.trim().length > 0 &&
      $loginForm.password.length > 0 &&
      !busy,
  );

  async function onConnect() {
    if (!canConnect) return;
    const { network, password, remember } = $loginForm;
    persistLogin(network.trim(), password, remember);
    await connect({ network: network.trim(), password });
  }
</script>

<div class="card">
  <header>
    <div class="brand">
      <Icon name="logo" size={20} />
      <span class="brand-name">{t.appName}</span>
    </div>
    <button class="icon-btn" aria-label={t.settings} onclick={() => openSettings("login")}>
      <Icon name="settings" />
    </button>
  </header>

  <div class="body">
    <div class="field">
      <label for="net">{t.networkLabel}</label>
      <input
        id="net"
        type="text"
        autocomplete="off"
        placeholder={t.networkPlaceholder}
        bind:value={$loginForm.network}
        onkeydown={(e) => e.key === "Enter" && onConnect()}
      />
    </div>

    <div class="field">
      <label for="pwd">{t.passwordLabel}</label>
      <div class="pwd-wrap">
        <input
          id="pwd"
          type={showPassword ? "text" : "password"}
          autocomplete="off"
          bind:value={$loginForm.password}
          onkeydown={(e) => e.key === "Enter" && onConnect()}
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
    </div>

    <p class="hint">{t.loginHint}</p>

    {#if errored}
      {@const m = errorMessage($status.error)}
      <div class="error" role="alert">
        <strong>{m.title}.</strong>
        {m.action}
      </div>
    {/if}

    <button class="primary connect" disabled={!canConnect} onclick={onConnect}>
      {busy ? t.connecting : t.connect}
    </button>

    <label class="remember">
      <input type="checkbox" bind:checked={$loginForm.remember} />
      {t.remember}
    </label>
  </div>
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
    align-items: center;
    justify-content: space-between;
    margin-bottom: 2rem;
  }
  .brand {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .brand-name {
    font-weight: 500;
    font-size: 16px;
  }
  .body {
    flex: 1;
    display: flex;
    flex-direction: column;
    justify-content: center;
    gap: 14px;
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
  .hint {
    font-size: 12px;
    color: var(--color-text-tertiary);
    margin: 2px 0 0;
    line-height: 1.5;
  }
  .connect {
    height: 40px;
    font-size: 15px;
    margin-top: 8px;
  }
  .remember {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 13px;
    color: var(--color-text-secondary);
    cursor: pointer;
  }
  .remember input {
    width: auto;
    height: auto;
    accent-color: var(--color-text-success);
  }
  .error {
    font-size: 13px;
    line-height: 1.5;
    color: var(--color-text-primary);
    background: color-mix(in srgb, var(--color-danger) 14%, transparent);
    border: 1px solid color-mix(in srgb, var(--color-danger) 40%, transparent);
    border-radius: var(--border-radius-md);
    padding: 10px 12px;
  }
</style>
