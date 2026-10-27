// The webview's view of the Rust core (src-tauri/src/commands.rs). The UI
// renders the Snapshot pushed in `session` events and sends commands; all
// logic lives in Rust.
import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'

export type TalkMode = 'push_to_talk' | 'voice_activity'

export type Phase =
  | { kind: 'idle' }
  | { kind: 'connecting' }
  | { kind: 'connected' }
  | { kind: 'reconnecting' }
  | { kind: 'failed'; reason: string; retryable: boolean }

export interface Member {
  id: number
  name: string
  muted: boolean
  deafened: boolean
  speaking: boolean
  volume: number
}

export interface Channel {
  id: number
  name: string
  members: Member[]
}

export interface Stage {
  name: string
  ms: number
  detail: string
  measured: boolean
}

export interface Snapshot {
  phase: Phase
  server: { name: string; address: string; version: string; protocol: number; online: number } | null
  channels: Channel[]
  me: number | null
  muted: boolean
  deafened: boolean
  talk: { mode: TalkMode; key: string; fallback_reason: string | null }
  admin: boolean
  rtt_ms: number | null
  latency: { total_ms: number; stages: Stage[] }
  audio_error: string | null
}

export interface Preferences {
  display_name: string
  input_device: string | null
  output_device: string | null
  talk_mode: TalkMode
  ptt_key: string
  beta_updates: boolean
}

export interface Device {
  id: string
  name: string
}

export type AdminAction =
  | { action: 'create_channel'; name: string }
  | { action: 'delete_channel'; channel_id: number }
  | { action: 'rename_channel'; channel_id: number; name: string }
  | { action: 'move_member'; session_id: number; channel_id: number }
  | { action: 'kick'; session_id: number }
  | { action: 'ban'; session_id: number }

export const api = {
  snapshot: () => invoke<Snapshot>('snapshot'),
  takePendingInvite: () => invoke<string | null>('take_pending_invite'),
  inviteAddress: (link: string) => invoke<string>('invite_address', { link }),
  join: (link: string, name: string) => invoke<void>('join', { link, name }),
  leave: () => invoke<void>('leave'),
  joinChannel: (channelId: number) => invoke<void>('join_channel', { channelId }),
  admin: (action: AdminAction) => invoke<void>('admin', { action }),
  createInvite: () => invoke<string>('create_invite'),
  setMuted: (muted: boolean) => invoke<void>('set_muted', { muted }),
  setDeafened: (deafened: boolean) => invoke<void>('set_deafened', { deafened }),
  setVolume: (sessionId: number, percent: number) =>
    invoke<void>('set_volume', { sessionId, percent }),
  devices: () => invoke<{ inputs: Device[]; outputs: Device[] }>('devices'),
  micTest: (on: boolean) => invoke<void>('mic_test', { on }),
  micLevel: () => invoke<number>('mic_level'),
  preferences: () => invoke<Preferences>('preferences'),
  setPreferences: (preferences: Preferences) => invoke<void>('set_preferences', { preferences }),
  capturePttKey: () => invoke<string | null>('capture_ptt_key'),
  cancelPttCapture: () => invoke<void>('cancel_ptt_capture'),
}

export function on<T>(event: string, handler: (payload: T) => void): Promise<UnlistenFn> {
  return listen<T>(event, (e) => handler(e.payload))
}

export const inCall = (s: Snapshot | null) =>
  s?.phase.kind === 'connected' || s?.phase.kind === 'reconnecting'

export const initials = (name: string) =>
  (name.trim() || '?')
    .split(/\s+/)
    .slice(0, 2)
    .map((w) => w[0].toUpperCase())
    .join('')

export const talkLabel = (t: Snapshot['talk']) =>
  t.mode === 'push_to_talk' && !t.fallback_reason ? `Push to talk · ${keyLabel(t.key)}` : 'Voice activity'

/// Push-to-talk input names (src-tauri/src/ptt.rs) for people: Mouse4 -> Mouse 4, KeyV -> V.
export const keyLabel = (key: string) =>
  key.replace(/^Mouse(\d+)$/, 'Mouse $1').replace(/^(Key|Num)(\w)$/, '$2')

/// Commands reject with a user-facing string.
export const message = (e: unknown) => (typeof e === 'string' ? e : String(e))
