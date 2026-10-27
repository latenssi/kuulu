<script lang="ts">
  import { onMount } from 'svelte'
  import { api, message, type Device, type Preferences, type Snapshot } from './lib/api'
  import Icon from './lib/Icon.svelte'

  let {
    snap,
    link,
    onCancel,
    onJoined,
  }: { snap: Snapshot; link: string | null; onCancel?: () => void; onJoined: () => void } = $props()

  const SEGMENTS = 24

  let prefs = $state<Preferences | null>(null)
  let inputs = $state<Device[]>([])
  let pasted = $state('')
  let address = $state<string | null>(null)
  let error = $state<string | null>(null)
  let lit = $state(0)

  const invite = $derived(link ?? pasted)
  const connecting = $derived(snap.phase.kind === 'connecting')
  const failure = $derived(snap.phase.kind === 'failed' ? snap.phase.reason : null)

  $effect(() => {
    const value = invite.trim()
    address = null
    error = null
    if (value) api.inviteAddress(value).then((a) => (address = a), (e) => (error = message(e)))
  })

  onMount(() => {
    api.preferences().then((p) => (prefs = p))
    api.devices().then((d) => (inputs = d.inputs))
    api.micTest(true).catch((e) => (error = message(e)))
    // dBFS from -60 to 0 across the meter.
    const timer = setInterval(async () => {
      const level = await api.micLevel()
      const db = 20 * Math.log10(Math.max(level, 1e-6))
      lit = Math.round(Math.min(Math.max((db + 60) / 60, 0), 1) * SEGMENTS)
    }, 50)
    return () => {
      clearInterval(timer)
      api.micTest(false)
    }
  })

  async function changeMic() {
    if (!prefs) return
    await api.setPreferences(prefs)
    await api.micTest(true).catch((e) => (error = message(e)))
  }

  async function join(e: SubmitEvent) {
    e.preventDefault()
    if (!prefs || !address) return
    error = null
    try {
      await api.setPreferences(prefs)
      await api.join(invite, prefs.display_name)
      onJoined()
    } catch (e) {
      error = message(e)
    }
  }
</script>

<div class="page">
  <div class="logo">kuulu</div>
  <form class="card" onsubmit={join}>
    <h1>Join {address ?? 'a voice server'}</h1>
    <p class="lead">No account, no email — pick a name and you're in.</p>

    {#if !link}
      <label for="link">Invite link</label>
      <input id="link" type="text" placeholder="kuulu://host:7700/join?t=…&fp=…" bind:value={pasted} />
    {/if}

    {#if prefs}
      <label for="name">Display name</label>
      <input
        id="name"
        type="text"
        placeholder="What your friends call you"
        maxlength="32"
        bind:value={prefs.display_name}
      />

      <label for="mic">Microphone</label>
      <select id="mic" bind:value={prefs.input_device} onchange={changeMic}>
        <option value={null}>System default</option>
        {#each inputs as d (d.id)}
          <option value={d.id}>{d.name}</option>
        {/each}
      </select>
      <div class="meter">
        {#each { length: SEGMENTS } as _, i}
          <span class:on={i < lit} style:height="{i < lit ? 4 + Math.min(i, 8) * 3 : 4}px"></span>
        {/each}
        <span class="mono">Say something</span>
      </div>

      <label for="mode">How you talk</label>
      <div class="pills" id="mode">
        <label class="pill">
          <input type="radio" bind:group={prefs.talk_mode} value="push_to_talk" /> Push to talk
        </label>
        <label class="pill">
          <input type="radio" bind:group={prefs.talk_mode} value="voice_activity" /> Voice activity
        </label>
      </div>
    {/if}

    {#if error || failure}
      <p class="error">{error ?? failure}</p>
    {/if}

    <div class="actions">
      <button class="primary" disabled={!address || !prefs?.display_name.trim() || connecting}>
        {connecting ? 'Connecting…' : 'Join voice'}
        <Icon name="forward" size={18} />
      </button>
      {#if onCancel}
        <button type="button" onclick={onCancel}>Cancel</button>
      {/if}
    </div>
    <p class="mono">Your identity key is created on this device and never leaves it.</p>
  </form>
  <p class="mono footer">Open source · run your own server</p>
</div>

<style>
  .page {
    max-width: 1000px;
    margin: 0 auto;
    padding: 40px 16px;
    text-align: center;
  }
  .logo {
    font-size: 56px;
    margin-bottom: 28px;
  }
  form {
    text-align: left;
    padding: 36px 40px;
  }
  h1 {
    font-size: 36px;
    font-weight: 400;
    margin: 0 0 12px;
  }
  .lead {
    color: var(--dim);
    font-size: 18px;
    margin: 0 0 8px;
  }
  label[for] {
    display: block;
    margin: 28px 0 12px;
  }
  .meter {
    display: flex;
    align-items: flex-end;
    gap: 4px;
    height: 32px;
    margin-top: 10px;
  }
  .meter span:not(.mono) {
    flex: 1;
    background: #3a3940;
    border-radius: 2px;
  }
  .meter span.on {
    background: #dcdbe0;
  }
  .meter .mono {
    margin-left: 14px;
    white-space: nowrap;
  }
  .actions {
    display: flex;
    gap: 12px;
    margin: 32px 0 20px;
  }
  .actions .primary {
    display: inline-flex;
    align-items: center;
    gap: 10px;
    font-size: 18px;
    padding: 14px 32px;
  }
  .footer {
    margin-top: 32px;
  }
</style>
