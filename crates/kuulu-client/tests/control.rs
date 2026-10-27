//! Channels, roles and admin actions: a real server (data directory, SQLite)
//! and headless clients in one process.

use kuulu_client::{Client, Error, Event, IdentityKey, Request, RequestError, ServerState};
use kuulu_proto::control::reject::Reason;
use kuulu_proto::identity::to_hex;
use kuulu_server::Server;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

/// A fresh data directory with a config on port 0.
fn data_dir(test: &str, admins: &[&IdentityKey], channels: &[&str]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kuulu-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_config(&dir, 0, 100, admins, channels);
    dir
}

fn write_config(
    dir: &Path,
    port: u16,
    max_users: usize,
    admins: &[&IdentityKey],
    channels: &[&str],
) {
    let admins: Vec<_> = admins
        .iter()
        .map(|k| format!("\"{}\"", to_hex(&k.public_key())))
        .collect();
    let mut text = format!(
        "port = {port}\nmax_users = {max_users}\nadmins = [{}]\n",
        admins.join(", ")
    );
    for (id, name) in (1..).zip(channels) {
        text += &format!("[[channels]]\nid = {id}\nname = \"{name}\"\n");
    }
    std::fs::write(dir.join("kuulu.toml"), text).unwrap();
}

/// Best effort: on Windows the still-running server keeps `kuulu.db` open.
fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// Starts the server in `dir`; returns it, its loopback address and fingerprint.
fn start(dir: &Path) -> (Arc<Server>, SocketAddr, String) {
    let server = Arc::new(Server::open(dir).unwrap());
    let addr = SocketAddr::from(([127, 0, 0, 1], server.local_addr().unwrap().port()));
    let fp = server.fingerprint().to_string();
    let run = server.clone();
    tokio::spawn(async move { run.run().await });
    (server, addr, fp)
}

async fn until(client: &Client, what: impl Fn(&ServerState) -> bool) {
    timeout(WAIT, async {
        while !what(&client.state()) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; state: {:#?}", client.state()));
}

fn channel_of(state: &ServerState, session_id: u32) -> Option<u32> {
    state.members.get(&session_id).map(|m| m.channel_id)
}

/// Sends `count` frames at the 10 ms frame rate.
async fn talk(client: &Client, count: u16) {
    let mut tick = tokio::time::interval(Duration::from_millis(10));
    for seq in 0..count {
        tick.tick().await;
        client.send(0, seq, 0, b"frame");
    }
}

/// Number of packets received until none arrive for 300 ms.
async fn heard(client: &Client) -> usize {
    let mut count = 0;
    while timeout(Duration::from_millis(300), client.recv())
        .await
        .is_ok()
    {
        count += 1;
    }
    count
}

#[tokio::test(flavor = "multi_thread")]
async fn channels_isolate_voice_and_switching_works() {
    let dir = data_dir("channels", &[], &["General", "Games"]);
    let (_server, addr, fp) = start(&dir);
    let a = Client::connect_as(addr, &fp, &IdentityKey::generate(), "Aino")
        .await
        .unwrap();
    // No display name, as clients from before display names sent.
    let b = Client::connect(addr, &fp).await.unwrap();
    let c = Client::connect_as(addr, &fp, &IdentityKey::generate(), "Cecilia")
        .await
        .unwrap();
    let c_id = c.session_id().unwrap();
    until(&a, |s| s.members.len() == 3).await;
    let state = a.state();
    assert_eq!(state.channels[&2], "Games");
    assert_eq!(state.members[&b.session_id().unwrap()].display_name, "");
    assert_eq!(state.members[&c_id].display_name, "Cecilia");

    let mut events = a.events();
    c.request(Request::JoinChannel(2)).await.unwrap();
    assert_eq!(channel_of(&c.state(), c_id), Some(2));
    let event = timeout(WAIT, events.recv()).await.unwrap().unwrap();
    assert!(matches!(event, Event::MemberUpdated(m) if m.session_id == c_id && m.channel_id == 2));
    assert_eq!(
        c.request(Request::JoinChannel(99)).await,
        Err(RequestError::NotFound)
    );

    let (_, at_b, at_c) = tokio::join!(talk(&a, 20), heard(&b), heard(&c));
    assert!(at_b > 0 && at_c == 0, "{at_b} {at_c}");
    let (_, at_a, at_b) = tokio::join!(talk(&c, 20), heard(&a), heard(&b));
    assert_eq!((at_a, at_b), (0, 0));

    c.request(Request::JoinChannel(1)).await.unwrap();
    let (_, at_c) = tokio::join!(talk(&a, 20), heard(&c));
    assert!(at_c > 0);
    cleanup(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_actions_need_the_admin_role() {
    let admin_key = IdentityKey::generate();
    let dir = data_dir("admin", &[&admin_key], &["General"]);
    let (_server, addr, fp) = start(&dir);
    let admin = Client::connect_as(addr, &fp, &admin_key, "admin")
        .await
        .unwrap();
    let member = Client::connect(addr, &fp).await.unwrap();
    let other_key = IdentityKey::generate();
    let other = Client::connect_as(addr, &fp, &other_key, "other")
        .await
        .unwrap();
    let (member_id, other_id) = (member.session_id().unwrap(), other.session_id().unwrap());

    for request in [
        Request::Kick(other_id),
        Request::Ban(other_id),
        Request::CreateChannel("x".into()),
        Request::DeleteChannel(1),
        Request::RenameChannel(1, "x".into()),
        Request::MoveMember {
            session_id: other_id,
            channel_id: 1,
        },
    ] {
        assert_eq!(
            member.request(request.clone()).await,
            Err(RequestError::Forbidden),
            "{request:?}"
        );
    }

    let mut events = member.events();
    admin
        .request(Request::CreateChannel("  Games\n".into()))
        .await
        .unwrap();
    let event = timeout(WAIT, events.recv()).await.unwrap().unwrap();
    assert_eq!(
        event,
        Event::ChannelCreated {
            id: 2,
            name: "Games".into()
        }
    );
    assert_eq!(
        admin.request(Request::CreateChannel(" ".into())).await,
        Err(RequestError::Invalid)
    );
    admin
        .request(Request::MoveMember {
            session_id: member_id,
            channel_id: 2,
        })
        .await
        .unwrap();
    until(&member, |s| channel_of(s, member_id) == Some(2)).await;
    admin
        .request(Request::RenameChannel(2, "Raid".into()))
        .await
        .unwrap();
    until(&member, |s| s.channels.get(&2).is_some_and(|n| n == "Raid")).await;

    // Deleting a channel moves its members to the first one.
    admin.request(Request::DeleteChannel(2)).await.unwrap();
    until(&member, |s| {
        s.channels.len() == 1 && channel_of(s, member_id) == Some(1)
    })
    .await;
    assert_eq!(
        admin.request(Request::DeleteChannel(1)).await,
        Err(RequestError::Invalid)
    );

    admin.request(Request::Kick(other_id)).await.unwrap();
    assert_eq!(
        timeout(WAIT, other.recv()).await.unwrap(),
        Err(Error::Kicked)
    );
    until(&member, |s| !s.members.contains_key(&other_id)).await;
    // A kick is not a ban.
    Client::connect_as(addr, &fp, &other_key, "other")
        .await
        .unwrap();
    cleanup(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn bans_and_channels_survive_restart() {
    let admin_key = IdentityKey::generate();
    let banned_key = IdentityKey::generate();
    let dir = data_dir("ban", &[&admin_key], &["General"]);
    let (server, addr, fp) = start(&dir);
    let admin = Client::connect_as(addr, &fp, &admin_key, "admin")
        .await
        .unwrap();
    let banned = Client::connect_as(addr, &fp, &banned_key, "troll")
        .await
        .unwrap();

    admin
        .request(Request::Ban(banned.session_id().unwrap()))
        .await
        .unwrap();
    assert_eq!(
        timeout(WAIT, banned.recv()).await.unwrap(),
        Err(Error::Rejected(Reason::Banned))
    );
    let rejoin = Client::connect_as(addr, &fp, &banned_key, "troll").await;
    assert_eq!(rejoin.err(), Some(Error::Rejected(Reason::Banned)));
    let config = std::fs::read(dir.join("kuulu.toml")).unwrap();
    admin
        .request(Request::CreateChannel("Kept".into()))
        .await
        .unwrap();
    admin.request(Request::DeleteChannel(2)).await.unwrap();
    admin
        .request(Request::CreateChannel("Kept".into()))
        .await
        .unwrap();
    drop(admin);
    // The server never writes its config.
    assert_eq!(std::fs::read(dir.join("kuulu.toml")).unwrap(), config);

    server.close().await;
    // Config channels only seed the first start.
    write_config(&dir, 0, 100, &[&admin_key], &["Ignored", "Also ignored"]);
    let (_server, addr, fp) = start(&dir);
    let rejoin = Client::connect_as(addr, &fp, &banned_key, "troll").await;
    assert_eq!(rejoin.err(), Some(Error::Rejected(Reason::Banned)));
    let client = Client::connect(addr, &fp).await.unwrap();
    let channels: Vec<_> = client.state().channels.into_iter().collect();
    // Deleted id 2 was not reused.
    assert_eq!(
        channels,
        [(1, "General".to_string()), (3, "Kept".to_string())]
    );
    cleanup(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_config_is_an_error() {
    let dir = data_dir("empty", &[], &["General"]);
    std::fs::write(dir.join("kuulu.toml"), "\n").unwrap();
    assert!(Server::open(&dir).is_err());
    cleanup(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn full_server_rejects() {
    let dir = data_dir("full", &[], &["General"]);
    write_config(&dir, 0, 2, &[], &["General"]);
    let (_server, addr, fp) = start(&dir);
    let _a = Client::connect(addr, &fp).await.unwrap();
    let b = Client::connect(addr, &fp).await.unwrap();
    let err = Client::connect(addr, &fp).await.err();
    assert_eq!(err, Some(Error::Rejected(Reason::ServerFull)));
    drop(b);
    // Room again once someone leaves.
    timeout(WAIT, async {
        while Client::connect(addr, &fp).await.is_err() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    cleanup(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn request_flood_disconnects_only_the_flooder() {
    let dir = data_dir("flood", &[], &["General"]);
    let (_server, addr, fp) = start(&dir);
    let calm = Client::connect(addr, &fp).await.unwrap();
    let flooder = Arc::new(Client::connect(addr, &fp).await.unwrap());
    let flooder_id = flooder.session_id().unwrap();
    let mut events = flooder.events();
    for _ in 0..(kuulu_server::CONTROL_BURST * 5) {
        let flooder = flooder.clone();
        tokio::spawn(async move { flooder.request(Request::JoinChannel(1)).await });
    }
    let disconnected = timeout(WAIT, async {
        while events.recv().await != Ok(Event::Disconnected) {}
    })
    .await;
    assert!(disconnected.is_ok());
    until(&calm, |s| !s.members.contains_key(&flooder_id)).await;
    assert_eq!(calm.request(Request::JoinChannel(1)).await, Ok(()));
    cleanup(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn reconnect_restores_channel_and_mute() {
    let dir = data_dir("restore", &[], &["General", "Games"]);
    let (server, addr, fp) = start(&dir);
    let b = Client::connect(addr, &fp).await.unwrap();
    b.request(Request::JoinChannel(2)).await.unwrap();
    b.set_self_mute(true);
    until(&b, |s| s.me().is_some_and(|me| me.self_mute)).await;

    // Restart on the same port.
    server.close().await;
    drop(server);
    write_config(&dir, addr.port(), 100, &[], &["General", "Games"]);
    let server = timeout(WAIT, async {
        loop {
            if let Ok(server) = Server::open(&dir) {
                break Arc::new(server);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let run = server.clone();
    tokio::spawn(async move { run.run().await });

    until(&b, |s| {
        s.me()
            .is_some_and(|me| me.channel_id == 2 && me.self_mute && !me.self_deaf)
    })
    .await;
    cleanup(&dir);
}
