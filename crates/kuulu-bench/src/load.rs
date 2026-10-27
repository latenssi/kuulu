//! Server load test: a real `kuulu-server` process pinned to one CPU
//! (`taskset -c 0`, approximating one vCPU) and N headless clients on
//! loopback, each sending a 10 ms voice packet with an Opus-sized synthetic
//! payload. Runs three scenarios, the overload last:
//! - everyone in one channel, everyone talking (the worst case: N × (N − 1)
//!   deliveries per frame, far more than the server queues per listener);
//! - everyone in one channel, 4 talking (what the server queues per listener);
//! - the clients spread over 10 channels, 2 talking in each (a busy evening).
//!
//! Latency is send → receive: the payload carries the send time, so it
//! includes both client QUIC stacks and the bench's scheduling on top of the
//! server's forwarding. Clients and server share one machine, so keep them
//! apart: `taskset -c 1-9 kuulu-bench load` leaves CPU 0 to the server.
//! Build release first: `cargo build --release -p kuulu-server -p kuulu-bench`.

use kuulu_client::{Client, Error, IdentityKey, Request};
use kuulu_proto::{flags, redundancy};
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, UdpSocket};
use std::path::Path;
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{fs, thread};

const USAGE: &str = "usage: kuulu-bench load [--clients N] [--seconds S]";
const CHANNELS: u32 = 10;
/// Talkers per channel in the spread scenario.
const SPREAD_TALKERS: usize = 2;
/// Simultaneous speakers the server's queue towards each listener holds a
/// frame of (`SERVER_SEND_BUFFER` in kuulu-proto); with more, it drops the
/// oldest by design.
const QUEUED_SPEAKERS: usize = 4;
/// One Opus frame of 10 ms at 32 kbit/s, sent with the previous one as
/// redundancy, as the client does.
const OPUS_FRAME: usize = 40;
const WARMUP: Duration = Duration::from_secs(2);
/// Time for the last packets of the window to arrive.
const GRACE: Duration = Duration::from_millis(500);
/// USER_HZ, the unit of CPU times in /proc; 100 on every Linux target.
const CLOCK_TICKS: f64 = 100.0;

pub fn run() -> ExitCode {
    let (clients, seconds) = match parse(std::env::args().skip(2)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("load: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let dir = std::env::temp_dir().join(format!("kuulu-load-{}", std::process::id()));
    let result = ServerProcess::start(&dir, clients).and_then(|mut server| {
        let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        let result = runtime.block_on(bench(&server, clients, Duration::from_secs(seconds)));
        server.stop();
        result
    });
    let _ = fs::remove_dir_all(&dir);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("load: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<(usize, u64), String> {
    let (mut clients, mut seconds) = (100, 10);
    while let Some(arg) = args.next() {
        let value = args.next().ok_or(format!("{arg} needs a value"))?;
        let bad = || format!("bad {arg} {value:?}");
        match arg.as_str() {
            "--clients" => clients = value.parse().ok().filter(|&n| n >= 2).ok_or_else(bad)?,
            "--seconds" => seconds = value.parse().ok().filter(|&n| n > 0).ok_or_else(bad)?,
            _ => return Err(format!("unknown option {arg:?}")),
        }
    }
    Ok((clients, seconds))
}

struct ServerProcess {
    child: Child,
    addr: SocketAddr,
    fingerprint: String,
    invite: String,
}

impl ServerProcess {
    /// Starts `kuulu-server` from next to this binary in a fresh `dir`, with
    /// room for `clients` and [`CHANNELS`] channels, and makes an invite
    /// for all of them.
    fn start(dir: &Path, clients: usize) -> Result<Self, String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let binary = exe.with_file_name("kuulu-server");
        if !binary.exists() {
            return Err(format!(
                "{} not found; build it: cargo build --release -p kuulu-server",
                binary.display()
            ));
        }
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        // A free port; the server binds it right after.
        let port = UdpSocket::bind("127.0.0.1:0")
            .and_then(|s| s.local_addr())
            .map_err(|e| e.to_string())?
            .port();
        let channels: String = (1..=CHANNELS)
            .map(|id| format!("[[channels]]\nid = {id}\nname = \"load {id}\"\n"))
            .collect();
        let config = format!(
            "name = \"load\"\nport = {port}\npublic_host = \"127.0.0.1\"\nmax_users = {clients}\n{channels}"
        );
        fs::write(dir.join("kuulu.toml"), config).map_err(|e| e.to_string())?;
        let log = fs::File::create(dir.join("server.log")).map_err(|e| e.to_string())?;

        let mut command = if Path::new("/proc").exists() && which("taskset") {
            let mut c = Command::new("taskset");
            c.args(["-c", "0"]).arg(&binary);
            c
        } else {
            eprintln!("taskset not found: the server is not pinned to one CPU");
            Command::new(&binary)
        };
        let mut child = command
            .arg(dir)
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .map_err(|e| e.to_string())?;
        let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();
        let mut fingerprint = None;
        for line in lines.by_ref() {
            let line = line.map_err(|e| e.to_string())?;
            if let Some(fp) = line.strip_prefix("certificate SHA-256: ") {
                fingerprint = Some(fp.to_string());
                break;
            }
        }
        // Keep draining, so the server never blocks on a full pipe.
        thread::spawn(move || lines.for_each(drop));
        let Some(fingerprint) = fingerprint else {
            return Err(format!(
                "server exited; see {}",
                dir.join("server.log").display()
            ));
        };

        let output = Command::new(&binary)
            .args(["invite", "--uses", &clients.to_string()])
            .arg(dir)
            .output()
            .map_err(|e| e.to_string())?;
        let link = String::from_utf8_lossy(&output.stdout);
        let invite = link
            .split_once("t=")
            .and_then(|(_, rest)| rest.split('&').next())
            .ok_or(format!(
                "invite failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ))?
            .to_string();
        Ok(Self {
            child,
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            fingerprint,
            invite,
        })
    }

    /// The server's own CPU time so far.
    fn cpu(&self) -> Duration {
        cpu_time(&format!("/proc/{}/stat", self.child.id())).unwrap_or_default()
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(program).exists()))
}

/// User + system CPU time from a `/proc/.../stat` file.
fn cpu_time(stat: &str) -> Option<Duration> {
    let text = fs::read_to_string(stat).ok()?;
    // Fields after the parenthesised command name; utime and stime are the
    // 14th and 15th fields of the whole line.
    let mut fields = text.rsplit_once(')')?.1.split_whitespace().skip(11);
    let ticks: u64 = fields.next()?.parse::<u64>().ok()? + fields.next()?.parse::<u64>().ok()?;
    Some(Duration::from_secs_f64(ticks as f64 / CLOCK_TICKS))
}

/// Datagrams the kernel dropped for full UDP buffers, all sockets of this
/// network namespace: `RcvbufErrors + SndbufErrors` of `/proc/net/snmp`.
fn udp_buffer_drops() -> u64 {
    let Ok(text) = fs::read_to_string("/proc/net/snmp") else {
        return 0;
    };
    let mut udp = text.lines().filter(|l| l.starts_with("Udp:"));
    let (Some(names), Some(values)) = (udp.next(), udp.next()) else {
        return 0;
    };
    names
        .split_whitespace()
        .zip(values.split_whitespace())
        .filter(|(name, _)| matches!(*name, "RcvbufErrors" | "SndbufErrors"))
        .filter_map(|(_, value)| value.parse::<u64>().ok())
        .sum()
}

async fn bench(server: &ServerProcess, n: usize, window: Duration) -> Result<(), String> {
    eprintln!("connecting {n} clients (the server admits 2 per second per IP after a burst of 10)");
    let mut clients = Vec::with_capacity(n);
    for i in 0..n {
        clients.push(Arc::new(connect(server, i).await?));
    }

    // The overload scenario runs last, so its aftermath cannot skew the others.
    let mut rows = Vec::new();
    let channels = spread(&clients, CHANNELS).await?;
    let name = format!("{CHANNELS} channels, {SPREAD_TALKERS} talking in each");
    rows.push(scenario(server, &clients, &channels, SPREAD_TALKERS, window, &name).await?);
    let channels = spread(&clients, 1).await?;
    let name = format!("1 channel, {QUEUED_SPEAKERS} talking");
    rows.push(scenario(server, &clients, &channels, QUEUED_SPEAKERS, window, &name).await?);
    let name = "1 channel, all talking";
    rows.push(scenario(server, &clients, &channels, n, window, name).await?);

    println!(
        "{n} clients, {}-s windows, server pinned to CPU 0; latency is send -> receive",
        window.as_secs()
    );
    println!(
        "{:<32} {:>8} {:>10} {:>10} {:>7} {:>8} {:>7} {:>7} {:>7} {:>8} {:>8}",
        "scenario",
        "talkers",
        "in pkt/s",
        "out pkt/s",
        "loss %",
        "UDP drop",
        "p50 ms",
        "p99 ms",
        "max ms",
        "srv CPU%",
        "bench CPU%"
    );
    for row in rows {
        println!("{row}");
    }
    Ok(())
}

/// Moves client `i` to channel `i % count + 1`; returns each channel's
/// members as indices into `clients`.
async fn spread(clients: &[Arc<Client>], count: u32) -> Result<Vec<Vec<usize>>, String> {
    let mut channels = vec![Vec::new(); count as usize];
    for (i, client) in clients.iter().enumerate() {
        let channel = i % channels.len();
        client
            .request(Request::JoinChannel(channel as u32 + 1))
            .await
            .map_err(|e| format!("join channel: {e}"))?;
        channels[channel].push(i);
    }
    Ok(channels)
}

async fn connect(server: &ServerProcess, i: usize) -> Result<Client, String> {
    // The rate limit admits one connection every 0.5 s; far longer means
    // the server is gone.
    let give_up = Instant::now() + Duration::from_secs(10);
    loop {
        match Client::connect_as(
            server.addr,
            &server.fingerprint,
            &IdentityKey::generate(),
            &format!("load {i}"),
            Some(&server.invite),
        )
        .await
        {
            Ok(client) => return Ok(client),
            // Refused by the handshake rate limit: try again shortly.
            Err(Error::Connection(_)) if Instant::now() < give_up => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Err(e) => return Err(format!("client {i}: {e}")),
        }
    }
}

struct Row {
    name: String,
    talkers: usize,
    sent: u64,
    expected: u64,
    received: u64,
    udp_drops: u64,
    /// Sorted, in microseconds.
    latency: Vec<u32>,
    server_cpu: f64,
    bench_cpu: f64,
    window: Duration,
}

impl std::fmt::Display for Row {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let secs = self.window.as_secs_f64();
        let percentile = |p: f64| {
            let i = ((self.latency.len() as f64 * p) as usize).min(self.latency.len() - 1);
            f64::from(self.latency[i]) / 1000.0
        };
        let (p50, p99, max) = if self.latency.is_empty() {
            (f64::NAN, f64::NAN, f64::NAN)
        } else {
            (percentile(0.5), percentile(0.99), percentile(1.0))
        };
        write!(
            f,
            "{:<32} {:>8} {:>10.0} {:>10.0} {:>7.2} {:>8} {:>7.2} {:>7.2} {:>7.2} {:>8.0} {:>8.0}",
            self.name,
            self.talkers,
            self.sent as f64 / secs,
            self.received as f64 / secs,
            100.0 * (1.0 - self.received as f64 / self.expected.max(1) as f64),
            self.udp_drops,
            p50,
            p99,
            max,
            100.0 * self.server_cpu,
            100.0 * self.bench_cpu,
        )
    }
}

/// Runs one scenario: `channels[c]` are the indices into `clients` in
/// channel `c`, of which the first `talking` talk; everyone listens.
/// Packets sent within the window count.
async fn scenario(
    server: &ServerProcess,
    clients: &[Arc<Client>],
    channels: &[Vec<usize>],
    talking: usize,
    window: Duration,
    name: &str,
) -> Result<Row, String> {
    eprintln!("{name}: {} s", (WARMUP + window).as_secs());
    let epoch = Instant::now();
    let start = epoch + WARMUP;
    let end = start + window;
    let in_window = move |nanos: u64| {
        let at = epoch + Duration::from_nanos(nanos);
        at >= start && at < end
    };

    // (talker, listeners): everyone else in the talker's channel listens.
    let talkers: Vec<_> = channels
        .iter()
        .flat_map(|members| {
            let listeners = members.len() as u64 - 1;
            members.iter().take(talking).map(move |&t| (t, listeners))
        })
        .collect();

    let receivers: Vec<_> = clients
        .iter()
        .map(|client| {
            let client = client.clone();
            tokio::spawn(async move {
                let mut latency = Vec::new();
                let deadline = tokio::time::sleep_until((end + GRACE).into());
                tokio::pin!(deadline);
                loop {
                    tokio::select! {
                        packet = client.recv() => {
                            let Ok(packet) = packet else { break };
                            let received = epoch.elapsed();
                            let Some(sent) = sent_at(&packet.payload) else { continue };
                            if in_window(sent) {
                                let micros = (received.as_nanos() as u64).saturating_sub(sent) / 1000;
                                latency.push(micros.min(u64::from(u32::MAX)) as u32);
                            }
                        }
                        _ = &mut deadline => break,
                    }
                }
                latency
            })
        })
        .collect();

    let senders: Vec<_> = talkers
        .iter()
        .map(|&(t, _)| {
            let client = clients[t].clone();
            tokio::spawn(async move {
                let mut ticks = tokio::time::interval(Duration::from_millis(10));
                let (mut seq, mut sent) = (0u16, 0u64);
                let mut payload = [0u8; 1 + 2 * OPUS_FRAME];
                let frame = [0x55u8; OPUS_FRAME];
                while Instant::now() < end {
                    ticks.tick().await;
                    let nanos = epoch.elapsed().as_nanos() as u64;
                    // The send time stands in for the start of the current frame.
                    let mut current = frame;
                    current[..8].copy_from_slice(&nanos.to_le_bytes());
                    let len = redundancy::write(&current, &frame, &mut payload).expect("fits");
                    let timestamp = u32::from(seq) * 480;
                    client.send(flags::REDUNDANT, seq, timestamp, &payload[..len]);
                    seq = seq.wrapping_add(1);
                    if in_window(nanos) {
                        sent += 1;
                    }
                }
                sent
            })
        })
        .collect();

    tokio::time::sleep_until(start.into()).await;
    let (cpu0, bench0, drops0) = (server.cpu(), bench_cpu(), udp_buffer_drops());
    tokio::time::sleep_until(end.into()).await;
    let (cpu1, bench1, drops1) = (server.cpu(), bench_cpu(), udp_buffer_drops());

    let (mut sent, mut expected) = (0, 0);
    for (handle, &(_, listeners)) in senders.into_iter().zip(&talkers) {
        let n = handle.await.map_err(|e| e.to_string())?;
        sent += n;
        expected += n * listeners;
    }
    let mut latency = Vec::new();
    for handle in receivers {
        latency.extend(handle.await.map_err(|e| e.to_string())?);
    }
    latency.sort_unstable();
    let secs = window.as_secs_f64();
    Ok(Row {
        name: name.into(),
        talkers: talkers.len(),
        sent,
        expected,
        received: latency.len() as u64,
        udp_drops: drops1 - drops0,
        latency,
        server_cpu: (cpu1 - cpu0).as_secs_f64() / secs,
        bench_cpu: (bench1 - bench0).as_secs_f64() / secs,
        window,
    })
}

/// Send time in nanoseconds since the scenario's epoch, from the current
/// frame of a redundant payload.
fn sent_at(payload: &[u8]) -> Option<u64> {
    let (current, _) = redundancy::split(payload)?;
    Some(u64::from_le_bytes(current.get(..8)?.try_into().ok()?))
}

fn bench_cpu() -> Duration {
    cpu_time("/proc/self/stat").unwrap_or_default()
}
