//! LaptopLink command-line interface.
//!
//! A thin, well-documented wrapper over `laptoplink-core`. Every subcommand
//! maps to a single core operation; the CLI owns only argument parsing,
//! progress rendering (human or JSON), and exit-code mapping.
//!
//! Exit codes (stable, for scripting):
//!   0  success
//!   1  generic/unexpected error
//!   2  usage error (bad arguments)
//!   3  authorization/rejection (peer not paired, rejected by peer)
//!   4  integrity failure (hash mismatch, corruption)
//!   5  transfer not found
//!   6  resource limit exceeded

mod progress;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use laptoplink_core::config::Config;
use laptoplink_core::identity::Fingerprint;
use laptoplink_core::manifest::SenderRecord;
use laptoplink_core::{recv, send, sync, Engine, Error};
use progress::CliSink;
use std::path::PathBuf;
use std::str::FromStr;

/// Default TCP port for LaptopLink transfers.
const DEFAULT_PORT: u16 = 9031;

#[derive(Parser)]
#[command(
    name = "laptoplink",
    version,
    about = "Secure, chunked, resumable file transfer between computers.",
    long_about = None,
)]
struct Cli {
    /// Emit machine-readable JSON (one event/object per line) instead of
    /// human-readable text.
    #[arg(long, global = true)]
    json: bool,

    /// Suppress progress output; only errors and final results are printed.
    #[arg(long, short, global = true)]
    quiet: bool,

    /// Verbose diagnostics (notes, per-file detail).
    #[arg(long, short, global = true)]
    verbose: bool,

    /// Override the config/data root (useful for tests and isolated setups).
    #[arg(long, global = true, value_name = "DIR")]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage this device's identity and paired peers.
    #[command(subcommand)]
    Device(DeviceCmd),

    /// Send one or more files/directories to a device.
    Send(SendArgs),

    /// One-way sync a source directory's contents to a device's destination.
    Sync(SyncArgs),

    /// Receive files (run a listening server).
    Receive(ReceiveArgs),

    /// Inspect and control transfers.
    #[command(subcommand)]
    Transfer(TransferCmd),

    /// Measure throughput with a local loopback transfer.
    Benchmark(BenchArgs),

    /// Diagnose the local environment and configuration.
    Doctor,
}

#[derive(Subcommand)]
enum DeviceCmd {
    /// Show this device's identity and fingerprint.
    Show,
    /// List paired peers.
    List,
    /// Approve (pair) a peer by fingerprint.
    Pair {
        /// 64-char hex fingerprint of the peer's static key.
        #[arg(long)]
        fingerprint: String,
        /// A human-friendly name for the peer.
        #[arg(long, default_value = "peer")]
        name: String,
    },
    /// Revoke a previously paired peer.
    Revoke {
        #[arg(long)]
        fingerprint: String,
    },
}

#[derive(Args)]
struct SendArgs {
    /// Files and/or directories to send.
    #[arg(required = true, value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// Target device address: `host` or `host:port` (default port 9031).
    #[arg(long = "to", value_name = "ADDR")]
    to: String,

    /// Override chunk size in bytes (otherwise chosen adaptively).
    #[arg(long)]
    chunk_size: Option<u32>,

    /// Reuse an existing transfer id (manual resume).
    #[arg(long)]
    id: Option<String>,
}

#[derive(Args)]
struct SyncArgs {
    /// Source directory whose *contents* are synced into the destination.
    #[arg(value_name = "SRC_DIR")]
    source: PathBuf,

    /// Target device address: `host` or `host:port` (default port 9031).
    /// The destination directory is the one the receiver was started with.
    #[arg(long = "to", value_name = "ADDR")]
    to: String,

    /// Show what would change without transferring or deleting anything.
    #[arg(long)]
    dry_run: bool,

    /// Delete destination files that are not present in the source
    /// (destructive; prompts for confirmation unless --yes).
    #[arg(long)]
    delete: bool,

    /// Exclude paths matching this glob (repeatable). Excluded files are also
    /// protected from --delete.
    #[arg(long, value_name = "GLOB")]
    exclude: Vec<String>,

    /// Compare files by content hash instead of size + mtime.
    #[arg(long)]
    checksum: bool,

    /// Skip the confirmation prompt for --delete.
    #[arg(long)]
    yes: bool,

    /// Override chunk size in bytes for transferred files.
    #[arg(long)]
    chunk_size: Option<u32>,
}

#[derive(Args)]
struct ReceiveArgs {
    /// Destination directory for received files.
    #[arg(long, value_name = "DIR")]
    dest: Option<PathBuf>,

    /// Address to bind: `host:port` (default 0.0.0.0:9031).
    #[arg(long, value_name = "ADDR")]
    bind: Option<String>,

    /// Trust-on-first-use: auto-pair unknown peers (pairing mode).
    #[arg(long)]
    accept_new: bool,

    /// Handle a single inbound transfer, then exit.
    #[arg(long)]
    once: bool,
}

#[derive(Subcommand)]
enum TransferCmd {
    /// List known sender-side transfers.
    List,
    /// Show one transfer's status.
    Status { transfer_id: String },
    /// Resume an interrupted transfer.
    Resume { transfer_id: String },
    /// Mark a transfer cancelled.
    Cancel { transfer_id: String },
}

#[derive(Args)]
struct BenchArgs {
    /// Size of the synthetic file to transfer, in MiB.
    #[arg(long, default_value_t = 256)]
    size_mib: u64,

    /// Override chunk size in bytes.
    #[arg(long)]
    chunk_size: Option<u32>,
}

fn main() {
    let cli = Cli::parse();
    let code = match run(&cli) {
        Ok(()) => 0,
        Err(err) => {
            report_error(&cli, &err);
            exit_code_for(&err)
        }
    };
    std::process::exit(code);
}

fn run(cli: &Cli) -> anyhow::Result<()> {
    let config = match &cli.home {
        Some(root) => Config::at_root(root),
        None => Config::discover().context("resolving config directory")?,
    };

    match &cli.command {
        Command::Device(cmd) => run_device(cli, &config, cmd),
        Command::Send(args) => run_send(cli, &config, args),
        Command::Sync(args) => run_sync(cli, &config, args),
        Command::Receive(args) => run_receive(cli, &config, args),
        Command::Transfer(cmd) => run_transfer(cli, &config, cmd),
        Command::Benchmark(args) => run_benchmark(cli, &config, args),
        Command::Doctor => run_doctor(cli, &config),
    }
}

fn engine(config: &Config) -> anyhow::Result<Engine> {
    Ok(Engine::new(config.clone(), &default_device_name())?)
}

fn run_device(cli: &Cli, config: &Config, cmd: &DeviceCmd) -> anyhow::Result<()> {
    let engine = engine(config)?;
    match cmd {
        DeviceCmd::Show => {
            let id = engine.identity();
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "name": id.name,
                        "fingerprint": id.fingerprint().full(),
                        "fingerprint_short": id.fingerprint().short(),
                    })
                );
            } else {
                println!("Device name : {}", id.name);
                println!("Fingerprint : {}", id.fingerprint().full());
                println!("Short       : {}", id.fingerprint().short());
                println!("Config dir  : {}", config.config_dir.display());
            }
        }
        DeviceCmd::List => {
            let peers = config.load_peers()?;
            let list: Vec<_> = peers.list().cloned().collect();
            if cli.json {
                println!("{}", serde_json::to_string(&list)?);
            } else if list.is_empty() {
                println!("No paired devices. Pair one with `laptoplink device pair`.");
            } else {
                for p in list {
                    println!(
                        "{}  {}  {}",
                        if p.revoked { "revoked" } else { "active " },
                        &p.fingerprint[..16],
                        p.name
                    );
                }
            }
        }
        DeviceCmd::Pair { fingerprint, name } => {
            let fp = Fingerprint::from_str(fingerprint)?;
            let mut peers = config.load_peers()?;
            peers.approve(&fp, name)?;
            println!("Paired '{name}' ({})", fp.short());
        }
        DeviceCmd::Revoke { fingerprint } => {
            let fp = Fingerprint::from_str(fingerprint)?;
            let mut peers = config.load_peers()?;
            if peers.revoke(&fp)? {
                println!("Revoked {}", fp.short());
            } else {
                println!("No such paired device: {}", fp.short());
            }
        }
    }
    Ok(())
}

fn run_send(cli: &Cli, config: &Config, args: &SendArgs) -> anyhow::Result<()> {
    let engine = engine(config)?;
    let target = normalize_addr(&args.to);
    let items = send::expand_sources(&args.paths)?;
    if items.is_empty() {
        anyhow::bail!("no regular files found in the given paths");
    }
    let mut sink = CliSink::new(cli.json, cli.quiet, cli.verbose);
    let summary = send::send(
        config,
        engine.identity(),
        &target,
        items,
        send::SendOptions {
            transfer_id: args.id.clone(),
            chunk_size: args.chunk_size,
            device_name: engine.identity().name.clone(),
        },
        &mut sink,
    )?;
    sink.finish_send(&summary);
    Ok(())
}

fn run_sync(cli: &Cli, config: &Config, args: &SyncArgs) -> anyhow::Result<()> {
    let engine = engine(config)?;
    let target = normalize_addr(&args.to);

    // Destructive deletion requires confirmation unless --dry-run or --yes.
    if args.delete && !args.dry_run && !args.yes {
        eprint!(
            "This will DELETE destination files not present in '{}'. Continue? [y/N] ",
            args.source.display()
        );
        use std::io::Write;
        std::io::stderr().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
            anyhow::bail!("aborted; no changes made");
        }
    }

    let mut sink = CliSink::new(cli.json, cli.quiet, cli.verbose);
    let summary = sync::sync(
        config,
        engine.identity(),
        &target,
        &args.source,
        sync::SyncOptions {
            delete: args.delete,
            dry_run: args.dry_run,
            checksum: args.checksum,
            excludes: args.exclude.clone(),
            chunk_size: args.chunk_size,
            device_name: engine.identity().name.clone(),
        },
        &mut sink,
    )?;
    sink.finish_sync(&summary, args.delete);
    Ok(())
}

fn run_receive(cli: &Cli, config: &Config, args: &ReceiveArgs) -> anyhow::Result<()> {
    let engine = engine(config)?;
    let dest = args
        .dest
        .clone()
        .unwrap_or_else(|| config.default_receive_dir());
    std::fs::create_dir_all(&dest).context("creating destination directory")?;
    let bind = args
        .bind
        .clone()
        .unwrap_or_else(|| format!("0.0.0.0:{DEFAULT_PORT}"));
    let mut sink = CliSink::new(cli.json, cli.quiet, cli.verbose);
    let opts = recv::ReceiveOptions {
        dest,
        accept_new: args.accept_new,
        device_name: engine.identity().name.clone(),
        once: args.once,
    };
    recv::listen(config, engine.identity(), &bind, &opts, &mut sink)?;
    Ok(())
}

fn run_transfer(cli: &Cli, config: &Config, cmd: &TransferCmd) -> anyhow::Result<()> {
    match cmd {
        TransferCmd::List => {
            let records = SenderRecord::list(&config.transfers_dir())?;
            if cli.json {
                println!("{}", serde_json::to_string(&records)?);
            } else if records.is_empty() {
                println!("No transfers recorded.");
            } else {
                for r in records {
                    let bytes: u64 = r.files.iter().map(|f| f.size).sum();
                    println!(
                        "{}  {:?}  {} file(s)  {}  -> {}",
                        &r.transfer_id[..16.min(r.transfer_id.len())],
                        r.state,
                        r.files.len(),
                        human_bytes(bytes),
                        r.target_addr
                    );
                }
            }
        }
        TransferCmd::Status { transfer_id } => {
            let rec = SenderRecord::load(&config.transfers_dir(), transfer_id)?
                .ok_or(Error::TransferNotFound(transfer_id.clone()))?;
            if cli.json {
                println!("{}", serde_json::to_string(&rec)?);
            } else {
                println!("Transfer : {}", rec.transfer_id);
                println!("State    : {:?}", rec.state);
                println!("Target   : {}", rec.target_addr);
                println!("Files    : {}", rec.files.len());
                for f in &rec.files {
                    println!("  {}  {}", human_bytes(f.size), f.rel_path);
                }
            }
        }
        TransferCmd::Resume { transfer_id } => {
            let engine = engine(config)?;
            let mut sink = CliSink::new(cli.json, cli.quiet, cli.verbose);
            let summary = send::resume(
                config,
                engine.identity(),
                transfer_id,
                &engine.identity().name,
                &mut sink,
            )?;
            sink.finish_send(&summary);
        }
        TransferCmd::Cancel { transfer_id } => {
            let mut rec = SenderRecord::load(&config.transfers_dir(), transfer_id)?
                .ok_or(Error::TransferNotFound(transfer_id.clone()))?;
            rec.save_to(&config.transfers_dir())?;
            rec.set_state_and_save(laptoplink_core::manifest::TransferState::Cancelled)?;
            println!("Cancelled {transfer_id}");
        }
    }
    Ok(())
}

/// Benchmark: spin up a one-shot receiver on an ephemeral port, generate a
/// synthetic file, send it over the real encrypted path, and report MiB/s.
fn run_benchmark(cli: &Cli, config: &Config, args: &BenchArgs) -> anyhow::Result<()> {
    use std::io::Write;
    use std::time::Instant;

    let engine = engine(config)?;
    let identity = engine.identity().clone();

    // Bind first so we can learn the chosen port.
    let listener = recv::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;

    // Receiver destination under a temp dir.
    let tmp = std::env::temp_dir().join(format!("laptoplink-bench-{}", std::process::id()));
    let dest = tmp.join("recv");
    std::fs::create_dir_all(&dest)?;

    // Synthetic source file.
    let src_dir = tmp.join("src");
    std::fs::create_dir_all(&src_dir)?;
    let src = src_dir.join("benchmark.bin");
    let size = args.size_mib * 1024 * 1024;
    write_random_file(&src, size)?;

    // Run the receiver in a background thread (one-shot, accept-new pairing).
    let recv_cfg = config.clone();
    let recv_id = identity.clone();
    let recv_dest = dest.clone();
    let handle = std::thread::spawn(move || {
        let opts = recv::ReceiveOptions {
            dest: recv_dest,
            accept_new: true,
            device_name: "bench-receiver".into(),
            once: true,
        };
        let mut sink = laptoplink_core::progress::NullSink;
        recv::serve_on(&listener, &recv_cfg, &recv_id, &opts, &mut sink)
    });

    // Give the listener a moment to be ready, then send.
    std::thread::sleep(std::time::Duration::from_millis(100));
    let mut sink = laptoplink_core::progress::NullSink;
    let start = Instant::now();
    let summary = send::send(
        config,
        &identity,
        &addr.to_string(),
        send::expand_sources(std::slice::from_ref(&src))?,
        send::SendOptions {
            transfer_id: None,
            chunk_size: args.chunk_size,
            device_name: identity.name.clone(),
        },
        &mut sink,
    )?;
    let elapsed = start.elapsed();
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("receiver thread panicked"))??;

    let secs = elapsed.as_secs_f64();
    let mib = summary.bytes_sent as f64 / (1024.0 * 1024.0);
    let throughput = mib / secs;

    let _ = std::fs::remove_dir_all(&tmp);

    if cli.json {
        println!(
            "{}",
            serde_json::json!({
                "bytes": summary.bytes_sent,
                "seconds": secs,
                "mib_per_s": throughput,
                "chunk_size": args.chunk_size,
            })
        );
    } else {
        println!(
            "Transferred {} in {:.2}s",
            human_bytes(summary.bytes_sent),
            secs
        );
        println!("Throughput  : {throughput:.1} MiB/s (loopback, encrypted + verified)");
    }
    let _ = std::io::stdout().flush();
    Ok(())
}

fn run_doctor(cli: &Cli, config: &Config) -> anyhow::Result<()> {
    let mut checks: Vec<(String, bool, String)> = Vec::new();

    let id_ok = config.identity_path().exists();
    checks.push((
        "identity".into(),
        true,
        if id_ok {
            format!("present at {}", config.identity_path().display())
        } else {
            "will be created on first use".into()
        },
    ));

    let engine = engine(config)?;
    checks.push((
        "fingerprint".into(),
        true,
        engine.identity().fingerprint().short(),
    ));

    let peers = config.load_peers()?;
    checks.push((
        "paired-peers".into(),
        true,
        format!("{}", peers.list().count()),
    ));

    // Can we bind the default port?
    let bind_ok = std::net::TcpListener::bind(("0.0.0.0", DEFAULT_PORT)).is_ok();
    checks.push((
        "bind-port".into(),
        bind_ok,
        if bind_ok {
            format!("port {DEFAULT_PORT} available")
        } else {
            format!("port {DEFAULT_PORT} is in use")
        },
    ));

    if cli.json {
        let arr: Vec<_> = checks
            .iter()
            .map(|(k, ok, v)| serde_json::json!({"check": k, "ok": ok, "detail": v}))
            .collect();
        println!("{}", serde_json::to_string(&arr)?);
    } else {
        println!("LaptopLink doctor:");
        for (k, ok, v) in &checks {
            println!("  [{}] {:<14} {}", if *ok { "ok" } else { "!!" }, k, v);
        }
    }
    Ok(())
}

// ---- helpers ------------------------------------------------------------

/// Append the default port if the user gave only a host.
fn normalize_addr(addr: &str) -> String {
    if addr.contains(':') {
        addr.to_string()
    } else {
        format!("{addr}:{DEFAULT_PORT}")
    }
}

/// Best-effort device name from the environment.
fn default_device_name() -> String {
    for var in ["LAPTOPLINK_NAME", "HOSTNAME", "HOST", "COMPUTERNAME"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return v;
            }
        }
    }
    "laptoplink-device".to_string()
}

/// Write `size` bytes of pseudo-random data to `path` in bounded-memory blocks.
fn write_random_file(path: &std::path::Path, size: u64) -> anyhow::Result<()> {
    use rand::RngCore;
    use std::io::Write;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut buf = vec![0u8; 1024 * 1024];
    let mut remaining = size;
    let mut rng = rand::rngs::OsRng;
    while remaining > 0 {
        let n = (buf.len() as u64).min(remaining) as usize;
        rng.fill_bytes(&mut buf[..n]);
        f.write_all(&buf[..n])?;
        remaining -= n as u64;
    }
    f.flush()?;
    Ok(())
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

fn report_error(cli: &Cli, err: &anyhow::Error) {
    if cli.json {
        eprintln!("{}", serde_json::json!({ "error": err.to_string() }));
    } else {
        eprintln!("error: {err:#}");
    }
}

/// Map core errors to stable exit codes where we can downcast.
fn exit_code_for(err: &anyhow::Error) -> i32 {
    if let Some(e) = err.downcast_ref::<Error>() {
        return match e {
            Error::PeerNotAuthorized(_) | Error::RejectedByPeer(_) => 3,
            Error::Integrity(_) => 4,
            Error::TransferNotFound(_) => 5,
            Error::LimitExceeded(_) => 6,
            _ => 1,
        };
    }
    1
}
