mod imm2;
mod rng;
mod ui;
mod x9;

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

/// Printed after `--help`: what the window does once it's open.
const WINDOW_HELP: &str = "\
In the window:
  Every key goes to the host, Esc included. Quit with the close button.

  IMM2 on macOS: Cmd is the Windows key, Cmd-Q quits, and the menu bar has:
    Keys  > Send Ctrl-Alt-Del, Send Alt-PrintScreen,
            Switch Virtual Console (Ctrl-Alt-F1..F12), Send Function Key,
            Magic SysRq (Alt-SysRq + b..z, 0..9; ⚠ = kills/reboots),
            Release All Keys, Check Host Keyboard LEDs,
            Reset Host USB (BMC re-plugs its virtual keyboard/mouse),
            Show Sent Keys (keycap overlay, bottom-right)
    Video > Colour Depth (Automatic, or 7..23-bit)

Factory logins are used unless you pass --user/--password:
  Supermicro X9  ADMIN / ADMIN
  IBM IMM2       USERID / PASSW0RD";

/// A native console for ancient Java iKVMs: Supermicro X9 (ATEN) and IBM IMM2 (Avocent).
#[derive(Parser)]
#[command(name = "ipmicow", version, after_help = WINDOW_HELP, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// BMC host name or address; the BMC family is detected from its open KVM port
    host: Option<String>,

    #[command(flatten)]
    login: Login,
}

#[derive(Args)]
struct Login {
    /// BMC login [default: the family's factory login]
    #[arg(long, global = true, env = "IPMICOW_USER", hide_env_values = true)]
    user: Option<String>,

    /// BMC password [default: the family's factory password]
    #[arg(long, global = true, env = "IPMICOW_PASSWORD", hide_env_values = true)]
    password: Option<String>,

    /// Log every key and button sent, and every control message the BMC sends back
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Open a Supermicro X9 (ATEN iKVM, TCP 5900) console
    X9 { host: String },
    /// Open an IBM IMM2 (Avocent DVC, TCP 3900) console
    Imm2 { host: String },
    /// Protocol tools for bringing up a new BMC (see docs/adding-a-bmc.md)
    #[command(subcommand)]
    Debug(Debug),
}

#[derive(Subcommand)]
enum Debug {
    /// X9: mint a token, connect, print the first video rects, save x9-frame0.bin
    X9Probe { host: String },
    /// IMM2: send one APCP connect request and dump the BMC's reply (no login)
    Imm2Probe { host: String },
    /// IMM2: rebuild both byte streams from a `socat -x -v` log (written next to it)
    Imm2Decap { log: String },
    /// IMM2: list the APCP / BEEF / VID message headers in a captured stream
    Imm2Walk { stream: String },
    /// IMM2: DVC-decode a captured server→client stream to a BMP (written next to it)
    Imm2Decode { stream: String },
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Family {
    X9,
    Imm2,
}

impl Family {
    fn factory_login(self) -> (&'static str, &'static str) {
        match self {
            Family::X9 => ("ADMIN", "ADMIN"),
            Family::Imm2 => ("USERID", "PASSW0RD"),
        }
    }
}

/// BMC login used to mint console tokens (and, on the IMM2, for virtual media).
#[derive(Clone)]
pub struct Creds {
    pub user: String,
    pub pass: String,
}

static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Seconds since ipmicow started, e.g. `+  12.345`, for verbose log lines, so
/// they can be lined up against the host's own log (anchor on a known event
/// such as a USB reset, which shows on both sides).
pub fn stamp() -> String {
    let t = START.get_or_init(std::time::Instant::now).elapsed();
    format!("+{:8.3}", t.as_secs_f64())
}

fn main() -> Result<()> {
    stamp(); // start the clock
    let cli = Cli::parse();
    let login = &cli.login;

    match cli.command {
        Some(Command::X9 { host }) => open(&host, Family::X9, login),
        Some(Command::Imm2 { host }) => open(&host, Family::Imm2, login),
        Some(Command::Debug(cmd)) => match cmd {
            Debug::X9Probe { host } => {
                let creds = creds(Family::X9, login);
                x9::debug::probe(&host, &creds.user, &creds.pass)
            }
            Debug::Imm2Probe { host } => imm2::debug::probe_apcp(&host),
            Debug::Imm2Decap { log } => imm2::debug::decap(&log),
            Debug::Imm2Walk { stream } => imm2::debug::walk(&stream),
            Debug::Imm2Decode { stream } => imm2::debug::decode_capture(&stream),
        },
        None => match cli.host {
            Some(host) => open(&host, detect(&host)?, login),
            None => {
                use clap::CommandFactory;
                Cli::command().print_help()?;
                std::process::exit(2);
            }
        },
    }
}

/// Open the live console window for `host`.
fn open(host: &str, family: Family, login: &Login) -> Result<()> {
    let creds = creds(family, login);
    match family {
        Family::X9 => x9::viewer::live_view(host, &creds.user, &creds.pass),
        Family::Imm2 => imm2::viewer::live_view(host, &creds, login.verbose),
    }
}

/// The login to use: whatever was given, else the family's factory default.
fn creds(family: Family, login: &Login) -> Creds {
    let (user, pass) = family.factory_login();
    Creds {
        user: login.user.clone().unwrap_or_else(|| user.to_string()),
        pass: login.password.clone().unwrap_or_else(|| pass.to_string()),
    }
}

/// Tell the two families apart by which KVM port accepts a TCP connection:
/// the IMM2 listens on 3900, the X9 on 5900. A bare connect-and-close is
/// harmless to both.
fn detect(host: &str) -> Result<Family> {
    for (port, family) in [(imm2::KVM_PORT, Family::Imm2), (x9::KVM_PORT, Family::X9)] {
        let addrs: Vec<_> =
            (host, port).to_socket_addrs().with_context(|| format!("resolving {host}"))?.collect();
        if addrs.iter().any(|a| TcpStream::connect_timeout(a, Duration::from_secs(2)).is_ok()) {
            eprintln!("[ipmicow] {host}: {family:?} (TCP {port} is open)");
            return Ok(family);
        }
    }
    bail!(
        "{host}: nothing answered on TCP {} (IMM2) or {} (X9); \
         name the family with `ipmicow imm2 {host}` or `ipmicow x9 {host}`",
        imm2::KVM_PORT,
        x9::KVM_PORT
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_shapes_parse() {
        let c = Cli::try_parse_from(["ipmicow", "bmc.example"]).unwrap();
        assert_eq!(c.host.as_deref(), Some("bmc.example"));
        assert!(c.command.is_none());

        let c = Cli::try_parse_from(["ipmicow", "imm2", "bmc.example", "--user", "admin", "-v"]).unwrap();
        assert!(matches!(c.command, Some(Command::Imm2 { .. })));
        assert_eq!(c.login.user.as_deref(), Some("admin"));
        assert!(c.login.verbose);

        let c = Cli::try_parse_from(["ipmicow", "debug", "imm2-walk", "c2s.bin"]).unwrap();
        assert!(matches!(c.command, Some(Command::Debug(Debug::Imm2Walk { .. }))));

        assert!(Cli::try_parse_from(["ipmicow", "debug", "imm2-walk"]).is_err());
    }

    #[test]
    fn factory_logins_fill_in_only_what_is_missing() {
        let login = Login { user: Some("ops".into()), password: None, verbose: false };
        let c = creds(Family::Imm2, &login);
        assert_eq!((c.user.as_str(), c.pass.as_str()), ("ops", "PASSW0RD"));
        let c = creds(Family::X9, &Login { user: None, password: None, verbose: false });
        assert_eq!((c.user.as_str(), c.pass.as_str()), ("ADMIN", "ADMIN"));
    }
}
