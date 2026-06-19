<script lang="ts">
  // Минимальный переключатель (свой CSS, без UI-кита).
  let {
    checked = $bindable(false),
    label = "",
    onchange,
  }: { checked?: boolean; label?: string; onchange?: (v: boolean) => void } = $props();

  function toggle() {
    checked = !checked;
    onchange?.(checked);
  }
</script>

<button
  type="button"
  class="toggle"
  class:on={checked}
  role="switch"
  aria-checked={checked}
  aria-label={label}
  onclick={toggle}
>
  <span class="knob"></span>
</button>

<style>
  .toggle {
    width: 36px;
    height: 20px;
    padding: 0;
    border-radius: 10px;
    border: none;
    background: var(--color-border-secondary);
    position: relative;
    flex-shrink: 0;
    transition: background 0.18s ease;
  }
  .toggle:hover:not(:disabled) {
    background: var(--color-border-primary);
  }
  .toggle.on,
  .toggle.on:hover {
    background: var(--color-text-success);
  }
  .knob {
    position: absolute;
    top: 2px;
    left: 2px;
    width: 16px;
    height: 16px;
    border-radius: 50%;
    background: #fff;
    transition: left 0.18s ease;
  }
  .toggle.on .knob {
    left: 18px;
  }
</style>
