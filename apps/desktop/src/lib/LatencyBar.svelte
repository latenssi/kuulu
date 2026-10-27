<script lang="ts">
  import type { Stage } from './api'

  let { stages, height = 6 }: { stages: Stage[]; height?: number } = $props()
</script>

<div class="bar" style:height="{height}px">
  {#each stages as s, i (s.name)}
    <span style:flex-grow={s.ms} style:opacity={1 - i * 0.15} title="{s.name} {s.ms.toFixed(1)} ms"></span>
  {/each}
</div>

<style>
  .bar {
    display: flex;
    gap: 4px;
  }
  span {
    flex-basis: 0;
    min-width: 6px;
    border-radius: 3px;
    background: #e6e5ea;
  }
</style>
