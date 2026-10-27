//! Server and headless clients in one process, exchanging synthetic frames.

use kuulu_client::{Client, VoicePacket, netem};
use kuulu_proto::flags;
use kuulu_server::{Identity, Server};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::timeout;

const FRAME: Duration = Duration::from_millis(10);

async fn start_server(addr: SocketAddr, identity: &Identity) -> Arc<Server> {
    let server = Arc::new(Server::bind(addr, identity).unwrap());
    let run = server.clone();
    tokio::spawn(async move { run.run().await });
    server
}

fn payload(seq: u16) -> Vec<u8> {
    format!("frame {seq}").into_bytes()
}

/// Sends `count` frames at the 10 ms frame rate.
async fn talk(client: &Client, count: u16) {
    let mut tick = tokio::time::interval(FRAME);
    for seq in 0..count {
        tick.tick().await;
        client.send(flags::REDUNDANT, seq, u32::from(seq) * 480, &payload(seq));
    }
}

/// Collects packets until none arrive for `quiet`.
async fn collect(client: &Client, quiet: Duration) -> Vec<VoicePacket> {
    let mut packets = Vec::new();
    while let Ok(packet) = timeout(quiet, client.recv()).await {
        packets.push(packet.unwrap());
    }
    packets
}

fn check(packets: &[VoicePacket], sender: u32) {
    for p in packets {
        assert_eq!(p.header.sender, sender);
        assert_eq!(p.header.flags, flags::REDUNDANT);
        assert_eq!(p.header.timestamp, u32::from(p.header.seq) * 480);
        assert_eq!(p.payload, payload(p.header.seq));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_talk_through_server() {
    let identity = Identity::generate();
    let server = start_server("127.0.0.1:0".parse().unwrap(), &identity).await;
    let addr = server.local_addr().unwrap();
    let fp = identity.fingerprint();
    let a = Client::connect(addr, &fp).await.unwrap();
    let b = Client::connect(addr, &fp).await.unwrap();

    let (_, received) = tokio::join!(talk(&a, 100), collect(&b, Duration::from_millis(500)));
    check(&received, a.session_id().unwrap());
    assert!(received.len() >= 98, "{}", received.len());

    // The other direction too, and nothing echoes back to the sender.
    let (_, received, echoed) = tokio::join!(
        talk(&b, 10),
        collect(&a, Duration::from_millis(500)),
        collect(&b, Duration::from_millis(500))
    );
    check(&received, b.session_id().unwrap());
    assert_eq!(received.len(), 10);
    assert!(echoed.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn home_network_profile() {
    let identity = Identity::generate();
    let server = start_server("127.0.0.1:0".parse().unwrap(), &identity).await;
    let addr = server.local_addr().unwrap();
    let fp = identity.fingerprint();
    let local = "127.0.0.1:0".parse().unwrap();
    let via_a = netem::spawn(local, addr, netem::Profile::HOME, 1)
        .await
        .unwrap();
    let via_b = netem::spawn(local, addr, netem::Profile::HOME, 2)
        .await
        .unwrap();
    let a = Client::connect(via_a, &fp).await.unwrap();
    let b = Client::connect(via_b, &fp).await.unwrap();

    let (_, received) = tokio::join!(talk(&a, 300), collect(&b, Duration::from_millis(500)));
    check(&received, a.session_id().unwrap());
    // Two hops at 1 % loss each: expect about 6 lost of 300.
    assert!(received.len() >= 280, "{}", received.len());
    let reordered = received
        .windows(2)
        .filter(|w| w[1].header.seq < w[0].header.seq)
        .count();
    // 10 ms frames with up to ±5 ms jitter per hop do arrive out of order.
    assert!(reordered > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn rate_limit_drops_floods() {
    let identity = Identity::generate();
    let server = start_server("127.0.0.1:0".parse().unwrap(), &identity).await;
    let addr = server.local_addr().unwrap();
    let fp = identity.fingerprint();
    let a = Client::connect(addr, &fp).await.unwrap();
    let b = Client::connect(addr, &fp).await.unwrap();

    // Four small packets every 10 ms: 400/s offered, few enough per tick to
    // fit the datagram buffers so only the server's limit drops them.
    let start = Instant::now();
    let flood = async {
        let mut tick = tokio::time::interval(FRAME);
        for seq in 0..400u16 {
            if seq % 4 == 0 {
                tick.tick().await;
            }
            a.send(0, seq, 0, b"x");
        }
    };
    let (_, received) = tokio::join!(flood, collect(&b, Duration::from_millis(300)));
    let elapsed = start.elapsed().as_secs_f64();
    let limit = f64::from(kuulu_server::RATE_LIMIT);
    let allowed = (limit / 10.0 + limit * elapsed).ceil() as usize;
    assert!(received.len() <= allowed, "{} > {allowed}", received.len());
    // The limit must still let a steady voice stream's worth through.
    assert!(received.len() as f64 >= 0.8 * limit * (elapsed - 0.3).min(1.0));
}

#[tokio::test(flavor = "multi_thread")]
async fn reconnects_after_server_restart() {
    let identity = Identity::generate();
    let server = start_server("127.0.0.1:0".parse().unwrap(), &identity).await;
    let addr = server.local_addr().unwrap();
    let fp = identity.fingerprint();
    let a = Client::connect(addr, &fp).await.unwrap();
    assert!(a.session_id().is_some());

    server.close().await;
    drop(server);
    let a = &a;
    let wait_for = |want: bool| async move {
        while a.session_id().is_some() != want {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    timeout(Duration::from_secs(5), wait_for(false))
        .await
        .unwrap();

    // The OS may need a moment to release the port.
    let server = loop {
        if let Ok(server) = Server::bind(addr, &identity) {
            break Arc::new(server);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let run = server.clone();
    tokio::spawn(async move { run.run().await });
    timeout(Duration::from_secs(10), wait_for(true))
        .await
        .unwrap();

    let b = Client::connect(addr, &fp).await.unwrap();
    let (_, received) = tokio::join!(talk(a, 10), collect(&b, Duration::from_millis(500)));
    assert_eq!(received.len(), 10);
}
