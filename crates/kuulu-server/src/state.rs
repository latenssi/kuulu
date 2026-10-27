//! In-memory server state: channels, members, admins and bans. Every change
//! is broadcast to all members, and sent to the store, while the lock is
//! held, so all clients and the database see changes in the same order.

use crate::store::{Loaded, Op, Store};
use bytes::Bytes;
use kuulu_proto::close;
use kuulu_proto::control::{
    self, ChannelDeleted, MemberLeft, ServerMessage, Welcome, client_message::Msg, reject::Reason,
    response::Status, server_message,
};
use kuulu_proto::frame::MAX_FRAME_LEN;
use kuulu_proto::identity::PublicKey;
use std::collections::{BTreeMap, HashMap, HashSet};
use tokio::sync::mpsc;

/// Longest display, channel or server name, in characters.
const MAX_NAME: usize = 32;
/// Most channels; admins cannot create more.
pub(crate) const MAX_CHANNELS: usize = 64;

// Worst-case protobuf sizes, so the Welcome snapshot always fits one control
// frame. A field costs a 1-byte tag plus its value; a u32 varint is at most
// 5 bytes, a name at most MAX_NAME 4-byte UTF-8 characters (128 B) behind a
// 2-byte length, and a nested message adds a tag and a length varint.
const NAME_FIELD: usize = 1 + 2 + 4 * MAX_NAME;
const U32_FIELD: usize = 1 + 5;
/// session_id, display_name, channel_id, self_mute, self_deaf; wrapped in Channel.
const MEMBER_BOUND: usize = 1 + 2 + (U32_FIELD + NAME_FIELD + U32_FIELD + 2 + 2);
/// id and name, wrapped in Welcome with up to a 3-byte length (2 MiB).
const CHANNEL_BOUND: usize = 1 + 3 + (U32_FIELD + NAME_FIELD);
/// ServerMessage wrapper, session_id, server_name, protocol_version, plus
/// 256 B for server_version and features (checked by a test).
const WELCOME_OVERHEAD: usize = (1 + 3) + U32_FIELD + NAME_FIELD + U32_FIELD + 256;
/// Most simultaneous members whose Welcome still fits [`MAX_FRAME_LEN`];
/// `max_users` in `kuulu.toml` is clamped to it.
pub(crate) const MAX_USERS: usize =
    (MAX_FRAME_LEN - WELCOME_OVERHEAD - MAX_CHANNELS * CHANNEL_BOUND) / MEMBER_BOUND;
const _: () = assert!(MAX_USERS >= 100, "the plan's sizing target must fit");

pub(crate) struct State {
    last_session_id: u32,
    channels: BTreeMap<u32, String>,
    /// Never reused, so a stale id cannot address a newer channel.
    next_channel_id: u64,
    members: HashMap<u32, Member>,
    max_users: usize,
    admins: HashSet<PublicKey>,
    banned: HashSet<PublicKey>,
    store: Store,
}

pub(crate) struct Member {
    conn: quinn::Connection,
    /// Bounded: a client too slow to read its control stream is disconnected
    /// and resynchronises from the next Welcome.
    control: mpsc::Sender<ServerMessage>,
    key: PublicKey,
    name: String,
    channel: u32,
    /// Also set while deafened.
    mute: bool,
    deaf: bool,
}

impl Member {
    pub fn new(
        conn: quinn::Connection,
        control: mpsc::Sender<ServerMessage>,
        key: PublicKey,
        name: &str,
    ) -> Self {
        Self {
            conn,
            control,
            key,
            name: clean_name(name),
            channel: 0,
            mute: false,
            deaf: false,
        }
    }

    fn to_proto(&self, session_id: u32) -> control::Member {
        control::Member {
            session_id,
            display_name: self.name.clone(),
            channel_id: self.channel,
            self_mute: self.mute,
            self_deaf: self.deaf,
        }
    }
}

impl State {
    pub fn new(loaded: Loaded, max_users: usize, admins: HashSet<PublicKey>, store: Store) -> Self {
        Self {
            last_session_id: 0,
            channels: loaded
                .channels
                .into_iter()
                .map(|c| (c.id, c.name))
                .collect(),
            next_channel_id: loaded.next_channel_id,
            members: HashMap::new(),
            max_users,
            admins,
            banned: loaded.banned,
            store,
        }
    }

    /// Adds `member` to the first channel, queues `welcome` (completed with
    /// the session id and channels) to it and announces it to everyone else.
    pub fn join(&mut self, mut member: Member, mut welcome: Welcome) -> Result<u32, Reason> {
        if self.banned.contains(&member.key) {
            return Err(Reason::Banned);
        }
        if self.members.len() >= self.max_users {
            return Err(Reason::ServerFull);
        }
        let id = self.next_session_id();
        member.channel = self.first_channel();
        self.store.write(Op::Seen(member.key, member.name.clone()));
        self.broadcast(server_message::Msg::MemberJoined(member.to_proto(id)));
        self.members.insert(id, member);
        welcome.session_id = id;
        welcome.channels = self
            .channels
            .iter()
            .map(|(&channel, name)| control::Channel {
                id: channel,
                name: name.clone(),
                members: self.members_in(channel),
            })
            .collect();
        let welcome = server_message::Msg::Welcome(welcome);
        // The queue is new, so it has room.
        let _ = self.members[&id]
            .control
            .try_send(ServerMessage { msg: Some(welcome) });
        Ok(id)
    }

    pub fn leave(&mut self, id: u32) {
        if self.members.remove(&id).is_some() {
            self.broadcast(server_message::Msg::MemberLeft(MemberLeft {
                session_id: id,
            }));
        }
    }

    /// Sends a voice packet from `from` to the rest of its channel, unless
    /// `from` is muted. Deafened members get nothing.
    pub fn forward(&self, from: u32, packet: &Bytes) {
        let Some(sender) = self.members.get(&from) else {
            return;
        };
        if sender.mute {
            return;
        }
        for (&id, m) in &self.members {
            if id != from && m.channel == sender.channel && !m.deaf {
                // Never blocks: quinn drops the oldest queued datagram instead.
                let _ = m.conn.send_datagram(packet.clone());
            }
        }
    }

    /// Applies a request from member `from`. Admin requests are authorised here.
    pub fn handle(&mut self, from: u32, msg: Msg) -> Result<(), Status> {
        let admin = self.admins.contains(&self.member(from)?.key);
        match msg {
            Msg::JoinChannel(m) => self.move_member(from, m.channel_id),
            Msg::SetVoiceState(m) => {
                let member = self.member_mut(from)?;
                let new = (m.self_mute || m.self_deaf, m.self_deaf);
                if (member.mute, member.deaf) != new {
                    (member.mute, member.deaf) = new;
                    self.updated(from);
                }
                Ok(())
            }
            _ if !admin => Err(Status::Forbidden),
            Msg::CreateChannel(m) => {
                let name = channel_name(&m.name)?;
                if self.channels.len() >= MAX_CHANNELS {
                    return Err(Status::Invalid);
                }
                let id = u32::try_from(self.next_channel_id).map_err(|_| Status::Invalid)?;
                self.next_channel_id += 1;
                self.channels.insert(id, name.clone());
                self.store.write(Op::CreateChannel(id, name.clone()));
                self.broadcast(server_message::Msg::ChannelCreated(channel(id, name)));
                Ok(())
            }
            Msg::DeleteChannel(m) => {
                self.channel(m.channel_id)?;
                if self.channels.len() == 1 {
                    return Err(Status::Invalid);
                }
                self.channels.remove(&m.channel_id);
                self.store.write(Op::DeleteChannel(m.channel_id));
                let first = self.first_channel();
                let moved: Vec<u32> = self
                    .members
                    .iter()
                    .filter(|(_, member)| member.channel == m.channel_id)
                    .map(|(&id, _)| id)
                    .collect();
                for id in moved {
                    self.move_member(id, first)?;
                }
                self.broadcast(server_message::Msg::ChannelDeleted(ChannelDeleted {
                    id: m.channel_id,
                }));
                Ok(())
            }
            Msg::RenameChannel(m) => {
                let name = channel_name(&m.name)?;
                self.channel(m.channel_id)?;
                self.channels.insert(m.channel_id, name.clone());
                self.store
                    .write(Op::RenameChannel(m.channel_id, name.clone()));
                self.broadcast(server_message::Msg::ChannelRenamed(channel(
                    m.channel_id,
                    name,
                )));
                Ok(())
            }
            Msg::MoveMember(m) => self.move_member(m.session_id, m.channel_id),
            Msg::Kick(m) => {
                let target = self.member(m.session_id)?;
                target.conn.close(close::KICKED.into(), b"kicked");
                Ok(())
            }
            Msg::Ban(m) => {
                let key = self.member(m.session_id)?.key;
                self.banned.insert(key);
                self.store.write(Op::Ban(key));
                for member in self.members.values().filter(|member| member.key == key) {
                    member.conn.close(close::BANNED.into(), b"banned");
                }
                Ok(())
            }
            Msg::Hello(_) => Err(Status::Invalid),
        }
    }

    fn move_member(&mut self, id: u32, channel: u32) -> Result<(), Status> {
        self.channel(channel)?;
        let member = self.member_mut(id)?;
        if member.channel != channel {
            member.channel = channel;
            self.updated(id);
        }
        Ok(())
    }

    fn updated(&self, id: u32) {
        let member = self.members[&id].to_proto(id);
        self.broadcast(server_message::Msg::MemberUpdated(member));
    }

    fn broadcast(&self, msg: server_message::Msg) {
        let msg = ServerMessage { msg: Some(msg) };
        for member in self.members.values() {
            if let Err(mpsc::error::TrySendError::Full(_)) = member.control.try_send(msg.clone()) {
                member.conn.close(0u32.into(), b"control stream too slow");
            }
        }
    }

    fn members_in(&self, channel: u32) -> Vec<control::Member> {
        let mut members: Vec<_> = self
            .members
            .iter()
            .filter(|(_, m)| m.channel == channel)
            .map(|(&id, m)| m.to_proto(id))
            .collect();
        members.sort_by_key(|m| m.session_id);
        members
    }

    fn first_channel(&self) -> u32 {
        *self.channels.keys().next().expect("at least one channel")
    }

    fn channel(&self, id: u32) -> Result<&String, Status> {
        self.channels.get(&id).ok_or(Status::NotFound)
    }

    fn member(&self, id: u32) -> Result<&Member, Status> {
        self.members.get(&id).ok_or(Status::NotFound)
    }

    fn member_mut(&mut self, id: u32) -> Result<&mut Member, Status> {
        self.members.get_mut(&id).ok_or(Status::NotFound)
    }

    /// Wraps after 2^32 joins, skipping 0 (unset) and ids still in use, so
    /// two live sessions never share an id.
    fn next_session_id(&mut self) -> u32 {
        let mut id = self.last_session_id;
        loop {
            id = id.wrapping_add(1);
            if id != 0 && !self.members.contains_key(&id) {
                self.last_session_id = id;
                return id;
            }
        }
    }
}

fn channel(id: u32, name: String) -> control::Channel {
    control::Channel {
        id,
        name,
        members: Vec::new(),
    }
}

/// At most [`MAX_NAME`] characters, without control or invisible format
/// characters (bidi overrides, zero-width), which could spoof other names.
pub(crate) fn clean_name(name: &str) -> String {
    name.chars()
        .filter(|&c| !c.is_control() && !is_format(c))
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_NAME)
        .collect()
}

/// Unicode general category Cf (format), as of Unicode 16.
fn is_format(c: char) -> bool {
    matches!(c as u32,
        0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x0890..=0x0891 | 0x08E2
        | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F
        | 0xFEFF | 0xFFF9..=0xFFFB | 0x110BD | 0x110CD | 0x13430..=0x1343F
        | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F)
}

pub(crate) fn channel_name(name: &str) -> Result<String, Status> {
    let name = clean_name(name);
    if name.is_empty() {
        Err(Status::Invalid)
    } else {
        Ok(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_name_strips_invisible_and_limits_length() {
        assert_eq!(clean_name(" a\u{202E}b\u{200B}c\u{FEFF}\n "), "abc");
        assert_eq!(clean_name("\u{200B} x"), "x");
        assert_eq!(clean_name(&"é".repeat(40)).chars().count(), MAX_NAME);
        assert_eq!(clean_name("Äänekoski 😀"), "Äänekoski 😀");
    }

    /// A full server with the longest names and largest ids still fits the
    /// Welcome into one frame.
    #[test]
    fn worst_case_welcome_fits_a_frame() {
        let name = || clean_name(&"😀".repeat(MAX_NAME + 5));
        assert_eq!(name().len(), 4 * MAX_NAME);
        let member = |i: usize| control::Member {
            session_id: u32::MAX - i as u32,
            display_name: name(),
            channel_id: u32::MAX,
            self_mute: true,
            self_deaf: true,
        };
        let mut channels: Vec<_> = (0..MAX_CHANNELS)
            .map(|i| control::Channel {
                id: u32::MAX - i as u32,
                name: name(),
                members: Vec::new(),
            })
            .collect();
        // All members in one channel is the largest single length prefix.
        channels[0].members = (0..MAX_USERS).map(member).collect();
        let welcome = Welcome {
            session_id: u32::MAX,
            server_version: env!("CARGO_PKG_VERSION").into(),
            channels,
            server_name: name(),
            protocol_version: u32::MAX,
            features: crate::FEATURES.iter().map(|f| f.to_string()).collect(),
        };
        let msg = ServerMessage {
            msg: Some(server_message::Msg::Welcome(welcome)),
        };
        let len = kuulu_proto::frame::encode(&msg).len() - 4;
        assert!(len <= MAX_FRAME_LEN, "{len}");
        // PLAN.md quotes this number.
        assert_eq!(MAX_USERS, 374);
        // The bound is tight enough to be useful.
        assert!(len > MAX_FRAME_LEN - 512 - MEMBER_BOUND, "{len}");
    }
}
