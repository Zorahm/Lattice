<script lang="ts">
  import "../lib/styles/app.css";
  import { onMount } from "svelte";
  import RoomsScreen from "../lib/components/RoomsScreen.svelte";
  import SettingsScreen from "../lib/components/SettingsScreen.svelte";
  import Icon from "../lib/components/Icon.svelte";
  import { screen } from "../lib/stores";
  import { initTheme, toggleTheme, theme } from "../lib/theme";
  import { startEventBridge, stopEventBridge, loadSettings } from "../lib/bridge";
  import { loadRooms } from "../lib/stores";

  onMount(() => {
    initTheme();
    loadRooms();
    loadSettings();
    startEventBridge();
    return () => stopEventBridge();
  });
</script>

<div class="app-root">
  <button class="theme-toggle" aria-label="Тема" onclick={toggleTheme}>
    <Icon name={$theme === "dark" ? "sun" : "moon"} size={16} />
  </button>

  <main class="stage">
    {#if $screen === "settings"}
      <SettingsScreen />
    {:else}
      <RoomsScreen />
    {/if}
  </main>
</div>

<style>
  .app-root {
    height: 100%;
    overflow-y: auto;
    overflow-x: hidden; /* страховка от случайного горизонтального скролла */
  }
  .stage {
    max-width: 440px;
    margin: 0 auto;
    padding: 1.25rem 1rem 4rem;
  }
  /* Переключатель темы — в пустом нижнем-правом углу: не конфликтует с
     иконкой настроек в шапках экранов и не налезает на скроллбар. */
  .theme-toggle {
    position: fixed;
    bottom: 14px;
    right: 18px;
    width: 32px;
    height: 32px;
    padding: 0;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: 50%;
    color: var(--color-text-tertiary);
    background: var(--color-background-secondary);
    border: 1px solid var(--color-border-primary);
    box-shadow: var(--shadow-pop);
    z-index: 20;
  }
  .theme-toggle:hover {
    color: var(--color-text-primary);
    background: var(--color-background-tertiary);
  }
</style>
