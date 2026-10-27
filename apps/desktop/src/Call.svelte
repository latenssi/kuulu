<script lang="ts">
  import { api, initials, message, talkLabel, type AdminAction, type Member, type Snapshot } from './lib/api'
  import Icon from './lib/Icon.svelte'
  import LatencyBar from './lib/LatencyBar.svelte'

  let {
    snap,
    version,
    pendingUpdate,
    onSettings,
  }: { snap: Snapshot; version: string; pendingUpdate: string | null; onSettings: () => void } = $props()

  let toast = $state<string | null>(null)
  let newChannel = $state('')
  let renaming = $state<number | null>(null)
  let renameTo = $state('')
  let menuFor = $state<number | null>(null)
  let confirmBan = $state(false)

  const myChannel = $derived(snap.channels.find((c) => c.members.some((m) => m.id === snap.me)))
  const self = $derived(myChannel?.members.find((m) => m.id === snap.me))
  const others = $derived(myChannel?.members.filter((m) => m.id !== snap.me) ?? [])

  async function run(p: Promise<unknown>) {
    try {
      await p
    } catch (e) {
      toast = message(e)
      setTimeout(() => (toast = null), 4000)
    }
  }

  const admin = (action: AdminAction) => run(api.admin(action))

  function status(m: Member) {
    if (m.deafened) return 'Deafened'
    if (m.muted) return 'Muted'
    return m.speaking ? 'Speaking' : 'Listening'
  }

  function rename(e: SubmitEvent, id: number) {
    e.preventDefault()
    if (renameTo.trim()) admin({ action: 'rename_channel', channel_id: id, name: renameTo.trim() })
    renaming = null
  }

  function create(e: SubmitEvent) {
    e.preventDefault()
    if (newChannel.trim()) admin({ action: 'create_channel', name: newChannel.trim() })
    newChannel = ''
  }
</script>

<div class="layout">
  <aside>
    <div class="brand">
      <span class="logo">kuulu</span>
      <span class="mono">{version}</span>
    </div>
    {#if snap.server}
      <div class="server">
        <div>{snap.server.name}</div>
        <div class="mono">{snap.server.address} · {snap.server.online} online</div>
      </div>
    {/if}

    <div class="label section">Voice channels</div>
    <nav>
      {#each snap.channels as c (c.id)}
        <div class="channel" class:current={c.id === myChannel?.id}>
          {#if renaming === c.id}
            <form onsubmit={(e) => rename(e, c.id)}>
              <!-- svelte-ignore a11y_autofocus -->
              <input type="text" bind:value={renameTo} autofocus onblur={() => (renaming = null)} />
            </form>
          {:else}
            <button
              class="row"
              onclick={() => c.id !== myChannel?.id && run(api.joinChannel(c.id))}
              ondblclick={() => {
                if (snap.admin) {
                  renaming = c.id
                  renameTo = c.name
                }
              }}
            >
              <Icon name="speaker" />
              <span class="grow">{c.name}</span>
              <span class="mono">{c.members.length}</span>
            </button>
            {#if snap.admin}
              <button
                class="icon-only"
                title="Delete channel"
                onclick={() => admin({ action: 'delete_channel', channel_id: c.id })}
              >
                <Icon name="x" size={14} />
              </button>
            {/if}
          {/if}
        </div>
        {#each c.members as m (m.id)}
          <div class="member">
            <span class="avatar" class:speaking={m.speaking} style:width="34px" style:height="34px"
              >{initials(m.name)}</span
            >
            <span class="grow">{m.name || 'Anonymous'}{m.id === snap.me ? ' (you)' : ''}</span>
            {#if m.muted || m.deafened}
              <span class="dim"><Icon name={m.deafened ? 'headphonesOff' : 'micOff'} size={16} /></span>
            {/if}
          </div>
        {/each}
      {/each}
    </nav>
    {#if snap.admin}
      <form class="new-channel" onsubmit={create}>
        <input type="text" placeholder="New channel" maxlength="32" bind:value={newChannel} />
      </form>
    {/if}
    <p class="mono foot">No account needed · double-click a channel to rename it</p>
  </aside>

  <main>
    <header>
      <h1>{myChannel?.name ?? ''}</h1>
      <span class="chip mono">OPUS · 10 MS FRAMES</span>
      <span class="chip mono">{myChannel?.members.length ?? 0} CONNECTED</span>
      <span class="grow"></span>
      {#if snap.phase.kind === 'reconnecting'}
        <span class="notice">Reconnecting…</span>
      {:else if pendingUpdate}
        <span class="notice">Update {pendingUpdate} installs after this call</span>
      {/if}
    </header>

    {#if snap.audio_error}
      <p class="error">{snap.audio_error}</p>
    {/if}

    <section class="cards">
      {#each others as m (m.id)}
        <article class="card">
          <div class="who">
            <span class="avatar big" class:speaking={m.speaking}>{initials(m.name)}</span>
            <div>
              <div class="name">{m.name || 'Anonymous'}</div>
              <div class="dim status">
                {#if m.muted || m.deafened}<Icon name={m.deafened ? 'headphonesOff' : 'micOff'} size={18} />{/if}
                {status(m)}
              </div>
            </div>
            {#if snap.admin}
              <button class="icon-only menu" title="Admin" onclick={() => (menuFor = menuFor === m.id ? null : m.id)}>
                <Icon name="more" />
              </button>
            {/if}
          </div>
          {#if menuFor === m.id}
            <div class="admin">
              <select
                onchange={(e) => {
                  admin({ action: 'move_member', session_id: m.id, channel_id: +e.currentTarget.value })
                  menuFor = null
                }}
              >
                <option value="" selected disabled>Move to…</option>
                {#each snap.channels.filter((c) => c.id !== myChannel?.id) as c (c.id)}
                  <option value={c.id}>{c.name}</option>
                {/each}
              </select>
              <button onclick={() => admin({ action: 'kick', session_id: m.id })}>Kick</button>
              <button
                class="danger"
                onclick={() => {
                  if (confirmBan) admin({ action: 'ban', session_id: m.id })
                  confirmBan = !confirmBan
                }}>{confirmBan ? 'Really ban?' : 'Ban'}</button
              >
            </div>
          {/if}
          <label class="volume mono">
            Volume
            <input
              type="range"
              min="0"
              max="200"
              step="5"
              value={m.volume}
              oninput={(e) => api.setVolume(m.id, +e.currentTarget.value)}
            />
            <span>{m.volume} %</span>
          </label>
        </article>
      {/each}
      {#if self}
        <article class="card">
          <div class="who">
            <span class="avatar big" class:speaking={self.speaking}>{initials(self.name)}</span>
            <div>
              <div class="name">{self.name || 'Anonymous'} (you)</div>
              <div class="dim status">{talkLabel(snap.talk)}</div>
            </div>
          </div>
          <p class="mono">RTT {snap.rtt_ms === null ? '—' : `${Math.round(snap.rtt_ms)} ms`}</p>
          <p class="mono">
            {#if snap.talk.fallback_reason}
              {snap.talk.fallback_reason}
            {:else if snap.talk.mode === 'push_to_talk'}
              Transmitting only while key is held
            {:else}
              Transmitting while you speak
            {/if}
          </p>
        </article>
      {/if}
    </section>

    <footer class="card">
      <span class="avatar" style:width="48px" style:height="48px">{initials(self?.name ?? '')}</span>
      <div>
        <div>{self?.name ?? ''}</div>
        <div class="mono">{talkLabel(snap.talk)}</div>
      </div>
      <span class="grow"></span>
      <div class="controls">
        <button
          class="square"
          class:active={snap.muted}
          title={snap.muted ? 'Unmute' : 'Mute'}
          onclick={() => run(api.setMuted(!snap.muted))}
        >
          <Icon name={snap.muted ? 'micOff' : 'mic'} size={24} />
        </button>
        <button
          class="square"
          class:active={snap.deafened}
          title={snap.deafened ? 'Undeafen' : 'Deafen'}
          onclick={() => run(api.setDeafened(!snap.deafened))}
        >
          <Icon name={snap.deafened ? 'headphonesOff' : 'headphones'} size={24} />
        </button>
        <button class="square" title="Settings" onclick={onSettings}><Icon name="gear" size={24} /></button>
        <button class="leave" onclick={() => api.leave()}>Leave</button>
      </div>
      <span class="grow"></span>
      <div class="latency" title="Estimate from the latency budget, measured RTT and jitter buffer">
        <div class="total">≈ {Math.round(snap.latency.total_ms)} ms</div>
        <LatencyBar stages={snap.latency.stages} height={5} />
        <div class="mono">Mouth to ear · estimate</div>
      </div>
    </footer>
  </main>
</div>

{#if toast}
  <div class="toast">{toast}</div>
{/if}

<style>
  .layout {
    display: grid;
    grid-template-columns: 340px 1fr;
    height: 100vh;
  }
  aside {
    display: flex;
    flex-direction: column;
    padding: 28px 24px;
    overflow-y: auto;
  }
  .brand {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    margin-bottom: 24px;
  }
  .server {
    background: var(--panel);
    border-radius: var(--radius);
    padding: 16px 20px;
    font-size: 18px;
  }
  .server .mono {
    margin-top: 6px;
  }
  .section {
    margin: 32px 0 12px;
  }
  .channel {
    display: flex;
    align-items: center;
    border-radius: 12px;
    color: var(--dim);
  }
  .channel.current {
    background: var(--panel);
    color: var(--text);
  }
  .channel .row {
    flex: 1;
    display: flex;
    align-items: center;
    gap: 14px;
    border: 0;
    border-radius: 12px;
    padding: 14px 16px;
    font-size: 19px;
    text-align: left;
  }
  .channel form {
    flex: 1;
    padding: 6px;
  }
  .icon-only {
    border: 0;
    padding: 8px;
    border-radius: 8px;
    color: var(--faint);
  }
  .channel .icon-only {
    visibility: hidden;
  }
  .channel:hover .icon-only {
    visibility: visible;
  }
  .member {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 6px 4px 6px 44px;
  }
  .new-channel {
    margin-top: 16px;
  }
  .foot {
    margin-top: auto;
    padding-top: 24px;
  }
  .grow {
    flex: 1;
  }
  .dim {
    color: var(--dim);
  }
  main {
    display: flex;
    flex-direction: column;
    padding: 28px 24px 24px 12px;
    min-width: 0;
  }
  header {
    display: flex;
    align-items: center;
    gap: 16px;
    flex-wrap: wrap;
    margin-bottom: 20px;
  }
  h1 {
    font-size: 40px;
    font-weight: 400;
    margin: 0 8px 0 0;
  }
  .chip {
    border: 1px solid var(--line);
    border-radius: 999px;
    padding: 6px 14px;
    letter-spacing: 1px;
  }
  .notice {
    color: var(--dim);
    font-size: 18px;
  }
  .cards {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(320px, 1fr));
    gap: 20px;
    align-content: start;
    flex: 1;
    overflow-y: auto;
  }
  .who {
    display: flex;
    align-items: center;
    gap: 20px;
  }
  .avatar.big {
    width: 76px;
    height: 76px;
    font-size: 24px;
  }
  .name {
    font-size: 22px;
  }
  .status {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-top: 6px;
    font-size: 17px;
  }
  .menu {
    margin-left: auto;
    align-self: flex-start;
  }
  .admin {
    display: flex;
    gap: 8px;
    margin-top: 16px;
  }
  .admin select {
    padding: 8px 10px;
  }
  .admin button {
    padding: 8px 14px;
  }
  .danger {
    color: var(--danger);
  }
  .volume {
    display: flex;
    align-items: center;
    gap: 16px;
    margin-top: 28px;
  }
  .volume input {
    flex: 1;
    accent-color: #f4f3f6;
  }
  footer {
    display: flex;
    align-items: center;
    gap: 18px;
    margin-top: 20px;
    padding: 20px 30px;
  }
  .controls {
    display: flex;
    gap: 12px;
  }
  .square {
    border-radius: 12px;
    padding: 14px;
    display: grid;
    place-items: center;
  }
  .square.active {
    background: #4a2e33;
    border-color: #6b3c43;
  }
  .leave {
    font-size: 18px;
    padding: 0 36px;
  }
  .latency {
    width: 220px;
    text-align: right;
  }
  .latency .total {
    font-size: 34px;
    margin-bottom: 6px;
  }
  .latency .mono {
    margin-top: 8px;
  }
  .toast {
    position: fixed;
    left: 50%;
    bottom: 140px;
    transform: translateX(-50%);
    background: #3a2a2e;
    border: 1px solid #6b3c43;
    padding: 12px 20px;
    border-radius: 12px;
  }
  @media (max-width: 1000px) {
    .layout {
      grid-template-columns: 260px 1fr;
    }
    footer {
      flex-wrap: wrap;
    }
  }
</style>
