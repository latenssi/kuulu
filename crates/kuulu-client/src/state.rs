//! The client's copy of the server state, kept current from Welcome and events.

use kuulu_proto::control::{self, Welcome, server_message::Msg};
use std::collections::BTreeMap;

/// What the client knows about the server. While reconnecting it keeps the
/// last known state with `session_id` `None`; a new Welcome replaces it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerState {
    /// Our session id while connected; it changes on reconnect.
    pub session_id: Option<u32>,
    pub server_name: String,
    pub server_version: String,
    /// Negotiated protocol major version.
    pub protocol_version: u32,
    pub features: Vec<String>,
    /// Channel names by id.
    pub channels: BTreeMap<u32, String>,
    /// Everyone connected, ourselves included, by session id.
    pub members: BTreeMap<u32, Member>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub session_id: u32,
    pub display_name: String,
    pub channel_id: u32,
    /// The server drops this member's voice. Set while deafened, too.
    pub self_mute: bool,
    /// This member hears nothing.
    pub self_deaf: bool,
    /// Has the admin role; can change at runtime (an `Event::MemberUpdated`).
    pub admin: bool,
}

/// A change to [`ServerState`], already applied when it is delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// (Re)connected: the state was replaced by a fresh snapshot.
    Connected,
    /// Connection lost; the client is reconnecting or has stopped.
    Disconnected,
    MemberJoined(Member),
    MemberLeft(Member),
    MemberUpdated(Member),
    ChannelCreated {
        id: u32,
        name: String,
    },
    ChannelDeleted {
        id: u32,
        name: String,
    },
    ChannelRenamed {
        id: u32,
        name: String,
    },
}

impl ServerState {
    /// Our own member entry while connected.
    pub fn me(&self) -> Option<&Member> {
        self.members.get(&self.session_id?)
    }

    pub(crate) fn from_welcome(welcome: Welcome) -> Self {
        let mut members = BTreeMap::new();
        let mut channels = BTreeMap::new();
        for channel in welcome.channels {
            for member in channel.members {
                members.insert(member.session_id, member.into());
            }
            channels.insert(channel.id, channel.name);
        }
        Self {
            session_id: Some(welcome.session_id),
            server_name: welcome.server_name,
            server_version: welcome.server_version,
            protocol_version: welcome.protocol_version,
            features: welcome.features,
            channels,
            members,
        }
    }

    /// Applies one server event; `None` for messages that change nothing.
    pub(crate) fn apply(&mut self, msg: Msg) -> Option<Event> {
        Some(match msg {
            Msg::MemberJoined(m) => {
                let m = Member::from(m);
                self.members.insert(m.session_id, m.clone());
                Event::MemberJoined(m)
            }
            Msg::MemberUpdated(m) => {
                let m = Member::from(m);
                self.members.insert(m.session_id, m.clone());
                Event::MemberUpdated(m)
            }
            Msg::MemberLeft(m) => Event::MemberLeft(self.members.remove(&m.session_id)?),
            Msg::ChannelCreated(c) => {
                self.channels.insert(c.id, c.name.clone());
                Event::ChannelCreated {
                    id: c.id,
                    name: c.name,
                }
            }
            Msg::ChannelRenamed(c) => {
                self.channels.insert(c.id, c.name.clone());
                Event::ChannelRenamed {
                    id: c.id,
                    name: c.name,
                }
            }
            Msg::ChannelDeleted(c) => Event::ChannelDeleted {
                id: c.id,
                name: self.channels.remove(&c.id)?,
            },
            Msg::Welcome(_) | Msg::Reject(_) | Msg::Response(_) => return None,
        })
    }
}

impl From<control::Member> for Member {
    fn from(m: control::Member) -> Self {
        Self {
            session_id: m.session_id,
            display_name: m.display_name,
            channel_id: m.channel_id,
            self_mute: m.self_mute,
            self_deaf: m.self_deaf,
            admin: m.admin,
        }
    }
}
