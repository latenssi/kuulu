<script lang="ts">
  import { getName, getVersion } from '@tauri-apps/api/app'
</script>

<main>
  {#await Promise.all([getName(), getVersion()]) then [name, version]}
    <h1>{name}</h1>
    <p>v{version}</p>
  {/await}
</main>

<style>
  main {
    font-family: system-ui, sans-serif;
    text-align: center;
    margin-top: 30vh;
  }
</style>
