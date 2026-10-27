//! Network emulator: a UDP proxy that adds delay, jitter and loss.
//!
//! Each packet gets an independent delay drawn from `delay ± jitter`, so
//! jitter larger than the packet interval also reorders packets, as real
//! networks do. Loss and delay draws come from a seeded RNG per direction,
//! so a given seed and packet sequence always gets the same treatment.

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::Instant;

/// Impairment applied to each direction separately.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Profile {
    pub delay: Duration,
    /// Maximum deviation from `delay`, uniformly distributed. At most `delay`.
    pub jitter: Duration,
    /// Probability in 0..=1 that a packet is dropped.
    pub loss: f64,
}

impl Profile {
    pub const LAN: Self = Self {
        delay: Duration::from_millis(1),
        jitter: Duration::ZERO,
        loss: 0.0,
    };
    pub const HOME: Self = Self {
        delay: Duration::from_millis(10),
        jitter: Duration::from_millis(5),
        loss: 0.01,
    };
    pub const POOR_MOBILE: Self = Self {
        delay: Duration::from_millis(40),
        jitter: Duration::from_millis(30),
        loss: 0.05,
    };
}

/// Starts a proxy on `listen` that forwards to `upstream`, and returns the
/// bound address. It serves one client: replies go to whoever sent last.
/// Runs until the tokio runtime shuts down.
pub async fn spawn(
    listen: SocketAddr,
    upstream: SocketAddr,
    profile: Profile,
    seed: u64,
) -> io::Result<SocketAddr> {
    assert!(profile.jitter <= profile.delay && (0.0..=1.0).contains(&profile.loss));
    let front = Arc::new(UdpSocket::bind(listen).await?);
    let back_bind: SocketAddr = if upstream.is_ipv6() {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    };
    let back = Arc::new(UdpSocket::bind(back_bind).await?);
    back.connect(upstream).await?;
    let local = front.local_addr()?;
    let client = Arc::new(Mutex::new(None));

    let mut up = Link::new(profile, seed);
    let (front_up, back_up, client_up) = (front.clone(), back.clone(), client.clone());
    tokio::spawn(async move {
        let mut buf = [0; 65536];
        while let Ok((len, from)) = front_up.recv_from(&mut buf).await {
            *client_up.lock().unwrap() = Some(from);
            let back = back_up.clone();
            up.forward(&buf[..len], move |packet| async move {
                let _ = back.send(&packet).await;
            });
        }
    });

    let mut down = Link::new(profile, seed.wrapping_add(1));
    tokio::spawn(async move {
        let mut buf = [0; 65536];
        while let Ok(len) = back.recv(&mut buf).await {
            let Some(to) = *client.lock().unwrap() else {
                continue;
            };
            let front = front.clone();
            down.forward(&buf[..len], move |packet| async move {
                let _ = front.send_to(&packet, to).await;
            });
        }
    });

    Ok(local)
}

struct Link {
    profile: Profile,
    rng: StdRng,
}

impl Link {
    fn new(profile: Profile, seed: u64) -> Self {
        Self {
            profile,
            rng: StdRng::seed_from_u64(seed),
        }
    }

    /// How long to hold the next packet, or `None` to drop it.
    fn delay(&mut self) -> Option<Duration> {
        let p = self.profile;
        // Draw both values for every packet so drops do not shift later draws.
        let lost = self.rng.random_bool(p.loss);
        let deviation = self.rng.random_range(-1.0..=1.0) * p.jitter.as_secs_f64();
        (!lost).then(|| Duration::from_secs_f64(p.delay.as_secs_f64() + deviation))
    }

    fn forward<F, Fut>(&mut self, packet: &[u8], send: F)
    where
        F: FnOnce(Vec<u8>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let Some(delay) = self.delay() else { return };
        let due = Instant::now() + delay;
        let packet = packet.to_vec();
        tokio::spawn(async move {
            tokio::time::sleep_until(due).await;
            send(packet).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draws(seed: u64) -> Vec<Option<Duration>> {
        let mut link = Link::new(Profile::POOR_MOBILE, seed);
        (0..10_000).map(|_| link.delay()).collect()
    }

    #[test]
    fn deterministic_and_within_profile() {
        assert_eq!(draws(7), draws(7));
        assert_ne!(draws(7), draws(8));
        let d = draws(7);
        let lost = d.iter().filter(|x| x.is_none()).count();
        assert!((400..=600).contains(&lost), "{lost}");
        let p = Profile::POOR_MOBILE;
        assert!(
            d.iter()
                .flatten()
                .all(|&x| x >= p.delay - p.jitter && x <= p.delay + p.jitter)
        );
    }

    #[tokio::test]
    async fn proxies_both_directions() {
        let echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0; 1500];
            while let Ok((len, from)) = echo.recv_from(&mut buf).await {
                echo.send_to(&buf[..len], from).await.unwrap();
            }
        });
        let proxy = spawn("127.0.0.1:0".parse().unwrap(), echo_addr, Profile::LAN, 1)
            .await
            .unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.send_to(b"ping", proxy).await.unwrap();
        let mut buf = [0; 16];
        let (len, from) = client.recv_from(&mut buf).await.unwrap();
        assert_eq!((&buf[..len], from), (&b"ping"[..], proxy));
    }
}
