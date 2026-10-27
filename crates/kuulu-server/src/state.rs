//! In-memory server state: channels, members, admins and bans. Every change
//! is broadcast to all members while the lock is held, so all clients see the
//! same changes in the same order.

use crate::config::ChannelConfig;
use bytes::Bytes;
use kuulu_proto::close;
use kuulu_proto::control::{
    self, ChannelDeleted, MemberLeft, ServerMessage, Welcome, client_message::Msg, reject::Reason,
    response::Status, server_message,
};
use kuulu_proto::identity::PublicKey;
use std::collections::{BTreeMap, HashMap, HashSet};
use tokio::sync::mpsc;

/// Longest display or channel name, in characters.
const MAX_NAME: usize = 32;

pub(crate) struct State {
    last_session_id: u32,
    channels: BTreeMap<u32, String>,
    members: HashMap<u32, Member>,
    admins: HashSet<PublicKey>,
    banned: HashSet<PublicKey>,
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

    /// Display name, cleaned of control characters and length-limited.
    pub fn name(&self) -> &str {
        &self.name
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

/// What a handled request changed that must also reach the disk.
pub(crate) enum Persist {
    Nothing,
    Channels,
    Ban(PublicKey),
}

impl State {
    pub fn new(
        channels: &[ChannelConfig],
        admins: HashSet<PublicKey>,
        banned: HashSet<PublicKey>,
    ) -> Self {
        Self {
            last_session_id: 0,
            channels: channels.iter().map(|c| (c.id, c.name.clone())).collect(),
            members: HashMap::new(),
            admins,
            banned,
        }
    }

    pub fn channels(&self) -> Vec<ChannelConfig> {
        self.channels
            .iter()
            .map(|(&id, name)| ChannelConfig {
                id,
                name: name.clone(),
            })
            .collect()
    }

    /// Adds `member` to the first channel, queues `welcome` (completed with
    /// the session id and channels) to it and announces it to everyone else.
    pub fn join(&mut self, mut member: Member, mut welcome: Welcome) -> Result<u32, Reason> {
        if self.banned.contains(&member.key) {
            return Err(Reason::Banned);
        }
        let id = self.next_session_id();
        member.channel = self.first_channel();
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
    pub fn handle(&mut self, from: u32, msg: Msg) -> Result<Persist, Status> {
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
                Ok(Persist::Nothing)
            }
            _ if !admin => Err(Status::Forbidden),
            Msg::CreateChannel(m) => {
                let name = channel_name(&m.name)?;
                let id = self.channels.keys().last().map_or(1, |id| id + 1);
                self.channels.insert(id, name.clone());
                self.broadcast(server_message::Msg::ChannelCreated(channel(id, name)));
                Ok(Persist::Channels)
            }
            Msg::DeleteChannel(m) => {
                self.channel(m.channel_id)?;
                if self.channels.len() == 1 {
                    return Err(Status::Invalid);
                }
                self.channels.remove(&m.channel_id);
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
                Ok(Persist::Channels)
            }
            Msg::RenameChannel(m) => {
                let name = channel_name(&m.name)?;
                self.channel(m.channel_id)?;
                self.channels.insert(m.channel_id, name.clone());
                self.broadcast(server_message::Msg::ChannelRenamed(channel(
                    m.channel_id,
                    name,
                )));
                Ok(Persist::Channels)
            }
            Msg::MoveMember(m) => self.move_member(m.session_id, m.channel_id),
            Msg::Kick(m) => {
                let target = self.member(m.session_id)?;
                target.conn.close(close::KICKED.into(), b"kicked");
                Ok(Persist::Nothing)
            }
            Msg::Ban(m) => {
                let key = self.member(m.session_id)?.key;
                self.banned.insert(key);
                for member in self.members.values().filter(|member| member.key == key) {
                    member.conn.close(close::BANNED.into(), b"banned");
                }
                Ok(Persist::Ban(key))
            }
            Msg::Hello(_) => Err(Status::Invalid),
        }
    }

    fn move_member(&mut self, id: u32, channel: u32) -> Result<Persist, Status> {
        self.channel(channel)?;
        let member = self.member_mut(id)?;
        if member.channel != channel {
            member.channel = channel;
            self.updated(id);
        }
        Ok(Persist::Nothing)
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

/// Printable characters only, at most [`MAX_NAME`].
fn clean_name(name: &str) -> String {
    name.trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME)
        .collect()
}

fn channel_name(name: &str) -> Result<String, Status> {
    let name = clean_name(name);
    if name.is_empty() {
        Err(Status::Invalid)
    } else {
        Ok(name)
    }
}
