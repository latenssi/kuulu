//! Kuulu server: forwards voice packets between channel members.
//!
//! The data directory (default: current directory) holds `kuulu.toml`,
//! `cert.der`, `key.der` and `kuulu.db`.

use kuulu_server::{Server, invite};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "usage:
  kuulu-server [DIR]                  run the server
  kuulu-server invite [--uses N] [--expires DURATION] [--host HOST] [DIR]
                                      create an invite link (default: 1 use, 7d)
  kuulu-server invite list [DIR]      show usable invites
  kuulu-server invite revoke ID [DIR] delete an invite
DURATION is a number with m, h or d, e.g. 30m, 12h, 7d.";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Run(PathBuf),
    Invite {
        dir: PathBuf,
        uses: u32,
        expires: Duration,
        host: Option<String>,
    },
    List(PathBuf),
    Revoke(PathBuf, i64),
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut dir = None;
    let mut set_dir = |arg: String| match dir.replace(PathBuf::from(&arg)) {
        None => Ok(()),
        Some(_) => Err(format!("unexpected argument {arg:?}")),
    };
    match args.next().as_deref() {
        Some("invite") => {}
        Some("-h" | "--help") => return Err(String::new()),
        Some(arg) => {
            set_dir(arg.into())?;
            return match args.next() {
                None => Ok(Command::Run(dir.unwrap())),
                Some(arg) => Err(format!("unexpected argument {arg:?}")),
            };
        }
        None => return Ok(Command::Run(".".into())),
    }
    let mut args = args.peekable();
    let sub = args.next_if(|a| a == "list" || a == "revoke");
    let id = match sub.as_deref() {
        Some("revoke") => {
            let id = args.next().ok_or("revoke needs an invite id")?;
            Some(id.parse().map_err(|_| format!("bad invite id {id:?}"))?)
        }
        _ => None,
    };
    let (mut uses, mut expires, mut host) = (1, Duration::from_secs(7 * 86400), None);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--uses" if sub.is_none() => {
                let v = value()?;
                uses = v
                    .parse()
                    .ok()
                    .filter(|&n| n > 0)
                    .ok_or(format!("bad --uses {v:?}"))?;
            }
            "--expires" if sub.is_none() => {
                let v = value()?;
                expires = parse_duration(&v).ok_or(format!("bad --expires {v:?}"))?;
            }
            "--host" if sub.is_none() => host = Some(value()?),
            _ if arg.starts_with('-') => return Err(format!("unknown option {arg:?}")),
            _ => set_dir(arg)?,
        }
    }
    let dir = dir.unwrap_or_else(|| ".".into());
    Ok(match (sub.as_deref(), id) {
        (Some("list"), _) => Command::List(dir),
        (_, Some(id)) => Command::Revoke(dir, id),
        _ => Command::Invite {
            dir,
            uses,
            expires,
            host,
        },
    })
}

/// `30m`, `12h` or `7d`; must be positive.
fn parse_duration(s: &str) -> Option<Duration> {
    let unit = match s.chars().last()? {
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        _ => return None,
    };
    let n: u64 = s[..s.len() - 1].parse().ok().filter(|&n| n > 0)?;
    Some(Duration::from_secs(n.checked_mul(unit)?))
}

/// Largest whole unit: `6d`, `23h` or `5m`.
fn format_duration(d: Duration) -> String {
    match d.as_secs() {
        s if s >= 86400 => format!("{}d", s / 86400),
        s if s >= 3600 => format!("{}h", s / 3600),
        s => format!("{}m", s / 60),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let command = match parse(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("error: {e}");
            }
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    match run(command).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(command: Command) -> std::io::Result<()> {
    match command {
        Command::Run(dir) => {
            let server = Server::open(&dir)?;
            println!("listening on UDP {}", server.local_addr()?);
            println!("certificate SHA-256: {}", server.fingerprint());
            if let Some(token) = server.admin_invite() {
                println!(
                    "no admin yet; whoever joins with this link (valid 24 h, one use) becomes one:"
                );
                println!("  {}", server.invite_link(token)?);
                println!("set public_host in kuulu.toml if the link shows <host>");
            }
            tokio::select! {
                _ = server.run() => {}
                _ = tokio::signal::ctrl_c() => server.close().await,
            }
        }
        Command::Invite {
            dir,
            uses,
            expires,
            host,
        } => println!("{}", invite::create(&dir, uses, expires, host.as_deref())?),
        Command::List(dir) => {
            for i in invite::list(&dir)? {
                let admin = if i.admin { "  (admin)" } else { "" };
                println!(
                    "{}  {} uses left  expires in {}{admin}",
                    i.id,
                    i.uses_left,
                    format_duration(i.expires_in)
                );
            }
        }
        Command::Revoke(dir, id) => {
            if !invite::revoke(&dir, id)? {
                return Err(std::io::Error::other(format!("no invite {id}")));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Result<Command, String> {
        parse(s.split_whitespace().map(String::from))
    }

    #[test]
    fn parses_commands() {
        assert_eq!(args(""), Ok(Command::Run(".".into())));
        assert_eq!(args("/srv"), Ok(Command::Run("/srv".into())));
        assert_eq!(
            args("invite --uses 10 --expires 7d"),
            Ok(Command::Invite {
                dir: ".".into(),
                uses: 10,
                expires: Duration::from_secs(7 * 86400),
                host: None,
            })
        );
        assert_eq!(
            args("invite /srv --host voice.example.com --expires 30m"),
            Ok(Command::Invite {
                dir: "/srv".into(),
                uses: 1,
                expires: Duration::from_secs(1800),
                host: Some("voice.example.com".into()),
            })
        );
        assert_eq!(args("invite list /srv"), Ok(Command::List("/srv".into())));
        assert_eq!(args("invite revoke 3"), Ok(Command::Revoke(".".into(), 3)));
        for bad in [
            "a b",
            "invite --uses 0",
            "invite --uses",
            "invite --expires 7",
            "invite --expires 0d",
            "invite --expires 7w",
            "invite --bogus",
            "invite a b",
            "invite revoke",
            "invite revoke x",
            "invite list --uses 2",
            "--help",
        ] {
            assert!(args(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("12h"), Some(Duration::from_secs(43200)));
        assert_eq!(parse_duration("h"), None);
        assert_eq!(parse_duration("-1d"), None);
        assert_eq!(format_duration(Duration::from_secs(7 * 86400 - 1)), "6d");
        assert_eq!(format_duration(Duration::from_secs(300)), "5m");
    }
}
