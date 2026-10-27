# Running a Kuulu server

`kuulu-server` is one binary that needs one open UDP port (7700 by default).
It keeps everything in one data directory:

| File | What it is |
| --- | --- |
| `kuulu.toml` | Configuration. Created with defaults on first start; the server never writes to it after that. |
| `cert.der`, `key.der` | TLS certificate and key. Clients pin the certificate's SHA-256 fingerprint. |
| `kuulu.db` (+ `-wal`, `-shm`) | SQLite database: channels, bans, invites, known identities, bootstrap admins. |

## Install

### Docker

```sh
docker run -d --name kuulu --restart unless-stopped \
  -p 7700:7700/udp -v kuulu:/data ghcr.io/latenssi/kuulu-server:<version>
```

Images exist for `linux/amd64` and `linux/arm64`. Tags:

- `X.Y.Z`, `X.Y.Z-beta.N`: one release. Pin one of these.
- `latest`: newest stable release. `beta`: newest beta.

The image runs as a non-root user (uid 65532) and keeps its data in the
`/data` volume.

### Docker Compose

[`deploy/docker-compose.yml`](../deploy/docker-compose.yml):

```sh
docker compose up -d
docker compose logs kuulu
```

Run the commands below with `docker compose exec kuulu kuulu-server ...`
instead of `docker exec kuulu kuulu-server ...`.

### Binary and systemd

Each [release](https://github.com/latenssi/kuulu/releases) has
`kuulu-server-<version>-linux-x86_64.tar.gz` and `-linux-aarch64.tar.gz`. The
binary needs glibc 2.36 or newer (Debian 12, Ubuntu 24.04, or newer).

```sh
useradd --system --home-dir /var/lib/kuulu-server --shell /usr/sbin/nologin kuulu
install -m 755 kuulu-server /usr/local/bin/
cp kuulu-server.service /etc/systemd/system/
systemctl enable --now kuulu-server
journalctl -u kuulu-server
```

[`kuulu-server.service`](../deploy/kuulu-server.service) keeps the data in
`/var/lib/kuulu-server` and sandboxes the process. Run the commands below as
`sudo -u kuulu kuulu-server ... /var/lib/kuulu-server`.

Without systemd: `kuulu-server [DIR]` runs the server with DIR (default: the
current directory) as the data directory. Stop it with Ctrl+C (SIGINT).

## First start

The log shows:

```
listening on UDP 0.0.0.0:7700
certificate SHA-256: cb9092a9...
no admin yet; whoever joins with this link (valid 24 h, one use) becomes one:
  kuulu://<host>:7700/join?t=280ff699...&fp=cb9092a9...
set public_host in kuulu.toml if the link shows <host>
```

1. Replace `<host>` in the link with the host name or IP address players use
   to reach the server, and open it in Kuulu. You are now the admin. A new
   start replaces the link; once an admin exists, no new one is made.
2. Set `public_host` in `kuulu.toml` so that invite links have the right
   address, then restart the server.
3. Optional: the log prints your admin key (`<hex> is now an admin`). Copy it
   to `admins` in `kuulu.toml` to keep it in the configuration file.

`docker logs kuulu` (or `journalctl -u kuulu-server`) shows the fingerprint and
link again later.

### Editing kuulu.toml

The image has no shell or editor. Either edit the file in the volume on the
host (`docker volume inspect kuulu` shows the path, usually
`/var/lib/docker/volumes/kuulu/_data`), or use a bind mount instead of a named
volume: `-v /srv/kuulu:/data` after `chown 65532:65532 /srv/kuulu`.
Restart the server after changes (`docker restart kuulu`).

| Setting | Default | |
| --- | --- | --- |
| `name` | `"Kuulu"` | Server name shown in clients |
| `port` | `7700` | UDP port, also used in invite links |
| `public_host` | `""` | Host name or IP address in invite links |
| `max_users` | `100` | Simultaneous users (at most 301) |
| `admins` | `[]` | Hex public keys of admins |
| `[[channels]]` | `General` | Channels created on the first start only; then admins manage them in the client |

An empty or invalid file is an error; the server does not fall back to
defaults. Delete the file to get a fresh default one.

## Invites

New users need an invite. Admins create them in the client, or on the server
(also while it runs):

```sh
docker exec kuulu kuulu-server invite --uses 10 --expires 7d
docker exec kuulu kuulu-server invite list
docker exec kuulu kuulu-server invite revoke 3
```

Defaults: one use, valid 7 days. `--host HOST` overrides `public_host` for
the link. Someone who has joined once can reconnect without an invite.

## Firewall and network

- Open **UDP** 7700 inbound (no TCP needed). Behind NAT, forward UDP 7700 to
  the server.
- To use another port, set `port` in `kuulu.toml` and publish the same port:
  `-p 7800:7800/udp`. Invite links use `port`, so mapping a different host
  port to 7700 gives links with the wrong port.
- The server listens on IPv4 only.
- Bandwidth: about 120 kbit/s per speaker per listener on the wire, so ten
  people with two talking at once need about 2.2 Mbit/s upload. Latency
  matters more than bandwidth: host close to the players.

## Backups

Back up the whole data directory. `cert.der` and `key.der` matter most: a new
certificate has a new fingerprint, and every client and invite link pinned to
the old one stops working.

`kuulu.db` is in WAL mode: recent changes may be only in `kuulu.db-wal`, so
copying `kuulu.db` alone from a running server can lose data. Either stop the
server for the copy (it takes a moment):

```sh
docker stop kuulu
docker run --rm -v kuulu:/data -v "$PWD":/backup debian:12-slim \
  tar -czf /backup/kuulu-$(date +%F).tar.gz -C /data .
docker start kuulu
```

or take a consistent copy online with the `sqlite3` tool, plus the other
files:

```sh
sqlite3 /var/lib/docker/volumes/kuulu/_data/kuulu.db ".backup kuulu-backup.db"
```

To restore, stop the server and put the files back, owned by uid 65532 for
the image or by `kuulu` for systemd.

## Upgrading

Change the image tag and recreate the container:

```sh
docker pull ghcr.io/latenssi/kuulu-server:<new version>
docker rm -f kuulu
docker run -d --name kuulu ... ghcr.io/latenssi/kuulu-server:<new version>
```

With Compose: change `image:` and `docker compose up -d`. With systemd:
replace the binary and `systemctl restart kuulu-server`. The data directory
stays as is; connected clients reconnect by themselves.

A bad release is fixed by a newer release; back up before upgrading if you
may want to go back.

## Privacy and security

- The server forwards voice; it never records or stores audio, and logs no
  message content.
- The log records connection events only: remote IP address and port, session
  id, joined or left, and the keys of new bootstrap admins.
- `kuulu.db` stores the public key, last display name and last-seen time of
  up to 10,000 most recently seen identities, plus channels, bans, admins,
  and invites (only a hash of each invite token).
- Connections are encrypted with QUIC (TLS 1.3), and clients pin the server's
  certificate. **There is no end-to-end encryption in v1:** the server
  decrypts voice to forward it, so whoever runs the server could listen in.
  Only use servers you trust.
