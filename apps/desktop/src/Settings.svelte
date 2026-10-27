<script lang="ts">
  import { onMount } from 'svelte'
  import { api, message, type Device, type Preferences, type Snapshot } from './lib/api'
  import Icon from './lib/Icon.svelte'
  import LatencyBar from './lib/LatencyBar.svelte'

  let { snap, version, onBack }: { snap: Snapshot; version: string; onBack: () => void } = $props()

  let prefs = $state<Preferences | null>(null)
  let devices = $state<{ inputs: Device[]; outputs: Device[] }>({ inputs: [], outputs: [] })
  let capturing = $state(false)
  let error = $state<string | null>(null)

  const channel = $derived(snap.channels.find((c) => c.members.some((m) => m.id === snap.me)))
  const sections = ['Audio and latency', 'Voice', 'Keybinds', 'Updates']
  const slug = (s: string) => s.toLowerCase().replaceAll(' ', '-')

  onMount(() => {
    api.preferences().then((p) => (prefs = p))
    api.devices().then((d) => (devices = d))
  })

  async function save() {
    if (!prefs) return
    error = null
    await api.setPreferences(prefs).catch((e) => (error = message(e)))
  }

  // Key names are DOM `code`s, which the global shortcut parser accepts.
  function captureKey(e: KeyboardEvent) {
    if (!capturing || !prefs) return
    e.preventDefault()
    if (e.code === 'Escape') {
      capturing = false
      return
    }
    if (/^(Control|Shift|Alt|Meta|OS)/.test(e.code)) return
    const mods = [e.ctrlKey && 'Ctrl', e.altKey && 'Alt', e.shiftKey && 'Shift', e.metaKey && 'Super']
    prefs.ptt_shortcut = [...mods.filter(Boolean), e.code].join('+')
    capturing = false
    save()
  }
</script>

<svelte:window onkeydown={captureKey} />

<div class="layout">
  <nav>
    <button class="back" onclick={onBack}>
      <Icon name="back" size={18} /> Back{channel ? ` to ${channel.name}` : ''}
    </button>
    {#each sections as s}
      <a href="#{slug(s)}">{s}</a>
    {/each}
  </nav>

  <main>
    <h1>Settings</h1>
    <p class="dim">Changes apply immediately, even mid-call.</p>
    {#if error}<p class="error">{error}</p>{/if}

    <section class="card" id="audio-and-latency">
      <div class="budget-head">
        <div>
          <h2>Latency budget</h2>
          <p class="mono">Mouth to ear · estimate from the budget and live RTT</p>
        </div>
        <div class="total">≈ {Math.round(snap.latency.total_ms)} ms</div>
      </div>
      <LatencyBar stages={snap.latency.stages} height={9} />
      <div class="stages">
        {#each snap.latency.stages as s (s.name)}
          <div>
            <div class="stage-name"><span class="dot"></span>{s.name}</div>
            <div class="mono ms">{s.ms.toFixed(1).replace(/\.0$/, '')} ms{s.measured ? '' : ' · budget'}</div>
            <div class="dim small">{s.detail}</div>
          </div>
        {/each}
      </div>
    </section>

    {#if prefs}
      <section class="card narrow">
        <h2>Devices</h2>
        <label for="in">Input</label>
        <select id="in" bind:value={prefs.input_device} onchange={save}>
          <option value={null}>System default</option>
          {#each devices.inputs as d (d.id)}<option value={d.id}>{d.name}</option>{/each}
        </select>
        <label for="out">Output</label>
        <select id="out" bind:value={prefs.output_device} onchange={save}>
          <option value={null}>System default</option>
          {#each devices.outputs as d (d.id)}<option value={d.id}>{d.name}</option>{/each}
        </select>
        <label for="buf">Audio buffer</label>
        <select id="buf" disabled>
          <option>240 samples · 5 ms (recommended)</option>
        </select>
        <p class="hint">Other buffer sizes and exclusive mode come later.</p>
        {#if snap.audio_error}<p class="error">{snap.audio_error}</p>{/if}
      </section>

      <section class="card narrow" id="voice">
        <h2>Voice</h2>
        <span class="field">Frame size</span>
        <div class="pills">
          <label class="pill"><input type="radio" disabled /> 5 ms</label>
          <label class="pill"><input type="radio" checked /> 10 ms (recommended)</label>
          <label class="pill"><input type="radio" disabled /> 20 ms</label>
        </div>
        <p class="hint">Fixed at 10 ms for now.</p>
        <span class="field">Jitter buffer</span>
        <div class="pills">
          <label class="pill"><input type="radio" checked /> Adaptive</label>
          <label class="pill"><input type="radio" disabled /> Fixed</label>
        </div>
        <p class="hint">Adaptive starts at one frame and grows only when the network needs it.</p>
        <label class="check">
          <input type="checkbox" disabled />
          <span>Noise suppression<br /><span class="hint">Coming later. Adds about 10 ms.</span></span>
        </label>
      </section>

      <section class="card narrow" id="keybinds">
        <h2>Talking</h2>
        <div class="pills">
          <label class="pill">
            <input type="radio" bind:group={prefs.talk_mode} value="push_to_talk" onchange={save} /> Push to talk
          </label>
          <label class="pill">
            <input type="radio" bind:group={prefs.talk_mode} value="voice_activity" onchange={save} /> Voice activity
          </label>
        </div>
        <span class="field">Push-to-talk key</span>
        <button onclick={() => (capturing = true)}>
          {capturing ? 'Press a key… (Esc cancels)' : prefs.ptt_shortcut}
        </button>
        <p class="hint">Keyboard keys only; mouse buttons are not supported yet.</p>
        {#if snap.talk.fallback_reason}<p class="error">{snap.talk.fallback_reason}</p>{/if}
      </section>

      <section class="card" id="updates">
        <h2>Updates</h2>
        <div>Version {version}</div>
        <p class="mono">Installs between calls, never during one.</p>
        <span class="field">Channel</span>
        <div class="pills">
          <label class="pill">
            <input type="radio" bind:group={prefs.beta_updates} value={false} onchange={save} /> Stable
          </label>
          <label class="pill">
            <input type="radio" bind:group={prefs.beta_updates} value={true} onchange={save} /> Beta
          </label>
        </div>
        <hr />
        <div class="signed">
          <Icon name="shield" />
          <div>
            Every update is signed. Kuulu checks the signature before installing anything.
            {#if snap.server}
              <div class="mono">
                Server {snap.server.address} · {snap.server.version} · protocol v{snap.server.protocol}
              </div>
            {/if}
          </div>
        </div>
      </section>
    {/if}
  </main>
</div>

<style>
  .layout {
    display: grid;
    grid-template-columns: 300px 1fr;
    height: 100vh;
  }
  nav {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 28px 24px;
  }
  nav a {
    color: var(--dim);
    text-decoration: none;
    padding: 12px 16px;
    border-radius: 10px;
    font-size: 18px;
  }
  nav a:hover {
    background: var(--panel);
    color: var(--text);
  }
  .back {
    display: flex;
    align-items: center;
    gap: 10px;
    border: 0;
    padding: 8px 0;
    margin-bottom: 28px;
    color: var(--dim);
    text-align: left;
  }
  main {
    padding: 28px 28px 60px 8px;
    overflow-y: auto;
  }
  main > section {
    margin-top: 20px;
    scroll-margin-top: 20px;
  }
  .narrow {
    max-width: 700px;
  }
  h1 {
    font-size: 36px;
    font-weight: 400;
    margin: 0;
  }
  h2 {
    font-size: 21px;
    font-weight: 500;
    margin: 0 0 8px;
  }
  .dim {
    color: var(--dim);
  }
  .small {
    font-size: 13px;
    line-height: 1.4;
  }
  .budget-head {
    display: flex;
    justify-content: space-between;
    margin-bottom: 20px;
  }
  .total {
    font-size: 52px;
  }
  .stages {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(140px, 1fr));
    gap: 20px;
    margin-top: 24px;
  }
  .stage-name {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .dot {
    width: 11px;
    height: 11px;
    border-radius: 3px;
    background: #cfcdd4;
  }
  .ms {
    color: var(--text);
    margin: 10px 0 6px;
  }
  label[for],
  .field {
    display: block;
    margin: 24px 0 10px;
  }
  .check {
    display: flex;
    gap: 12px;
    margin-top: 24px;
  }
  .check input {
    width: 20px;
    height: 20px;
  }
  hr {
    border: 0;
    border-top: 1px solid var(--line);
    margin: 24px 0;
  }
  .signed {
    display: flex;
    gap: 14px;
  }
  .signed .mono {
    margin-top: 8px;
  }
</style>
