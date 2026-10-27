<script lang="ts">
  import { onMount } from 'svelte'
  import { getVersion } from '@tauri-apps/api/app'
  import { api, inCall, on, type Snapshot } from './lib/api'
  import Join from './Join.svelte'
  import Call from './Call.svelte'
  import Settings from './Settings.svelte'

  let snap = $state<Snapshot | null>(null)
  let version = $state('')
  let settingsOpen = $state(false)
  // A deep-linked invite to show, even mid-call (joining it leaves the call).
  let invite = $state<string | null>(null)
  let pendingUpdate = $state<string | null>(null)

  onMount(() => {
    const unlisten = [
      on<Snapshot>('session', (s) => (snap = s)),
      on<string>('invite', (link) => {
        invite = link
        settingsOpen = false
      }),
      on<string>('update-pending', (v) => (pendingUpdate = v)),
      // Who is speaking, ten times a second; full snapshots come each second.
      on<number[]>('speaking', (ids) => {
        for (const m of snap?.channels.flatMap((c) => c.members) ?? []) m.speaking = ids.includes(m.id)
      }),
    ]
    getVersion().then((v) => (version = v))
    api.snapshot().then((s) => (snap ??= s))
    api.takePendingInvite().then((link) => (invite ??= link))
    return () => unlisten.forEach((u) => u.then((f) => f()))
  })
</script>

{#if snap}
  {#if settingsOpen}
    <Settings {snap} {version} onBack={() => (settingsOpen = false)} />
  {:else if inCall(snap) && !invite}
    <Call {snap} {version} {pendingUpdate} onSettings={() => (settingsOpen = true)} />
  {:else}
    <Join
      {snap}
      link={invite}
      onCancel={inCall(snap) ? () => (invite = null) : undefined}
      onJoined={() => (invite = null)}
    />
  {/if}
{/if}
