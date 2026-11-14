//! `castcorder-host`: the M1 headless host for a call on one local network.
//!
//! It serves the Browser Guest (the castcorder-client trunk build) over HTTPS
//! on every interface with a self-signed certificate (browsers grant the
//! microphone only in a secure context), and over plain HTTP on localhost for
//! the host machine's own browser, which is also the only page that gets the
//! producer controls (start and stop a take, the connected guests). Guests'
//! audio and video are forwarded by castcorder-transport on one fixed UDP port;
//! their finished tracks and log files are pulled into the library:
//! `<library>/<call>/takeNN/...` and `<library>/<call>/logs/...`, beside the
//! host's own rotating JSON-lines log.
//!
//! Usage: `castcorder-host --dist DIR [--library DIR] [--call NAME]
//! [--bind IP] [--port N] [--http-port N] [--udp-port N] [--advertise IP]...
//! [--rendezvous URL] [--relay-only] [--bandwidth-cap KBPS] [--participate [DEVICE]] [--name NAME] [--open]
//! [--record-video true|false]`
//!
//! Prints the join links, then `HOST_READY http=URL https=URL udp=ADDR` (the
//! line tests wait for). Everything printed is ASCII.
//!
//! With `--rendezvous URL` (none by default) the host also registers with
//! that rendezvous under its host key and, once registered, prints the call
//! link for guests anywhere and `CALL_LINK <link>` (see [`rendezvous`]). The
//! LAN pages and `POST /offer` keep working beside it.
//!
//! `--bandwidth-cap KBPS` caps the bandwidth the video allocation assumes
//! toward each guest (castcorder-transport `HostConfig::bandwidth_cap_kbps`):
//! for a thin uplink, and for testing the low layer. It shapes no traffic.
//!
//! `--record-video true|false` sets whether the call's takes record video
//! (browser guests' webcams; saved in `room.json`, default false). A producer
//! can change it in the call; it applies from the next take start.
//!
//! `--relay-only` (for testing the relay path) offers guests only the TURN
//! relayed candidate from the rendezvous: no host or server-reflexive ones.
//! Offers are refused until the relay is allocated.
//!
//! `--participate [DEVICE]` (builds with the `native-capture` feature only)
//! makes the person at this machine a participant without a browser: the
//! named input (default: the default microphone) is heard by every guest and
//! recorded into each take as `takeNN_<name>_mic.wav` (then FLAC), and the
//! guests are played on the default output. `--name NAME` is their display
//! name (default `host`). The track is an item of the take manifest under the
//! host's own device id. `--keep-wav false` deletes each WAV once its FLAC is
//! verified (both are kept by default). At startup, host-mic WAVs a previous
//! run left unfinalised are recovered (see `local`). See `native`.

use castcorder_host::Config;

fn fail(msg: &str) -> ! {
    eprintln!("castcorder-host: {msg}");
    eprintln!("usage: castcorder-host {}", castcorder_host::USAGE);
    std::process::exit(2);
}

fn parse_args() -> Config {
    let mut a = Config::new(std::path::PathBuf::new());
    a.log = true;
    let rest = a
        .apply_args(std::env::args().skip(1))
        .unwrap_or_else(|e| fail(&e));
    if let Some(flag) = rest.first() {
        if flag == "-h" || flag == "--help" {
            println!("usage: castcorder-host {}", castcorder_host::USAGE);
            std::process::exit(0);
        }
        fail(&format!("unknown argument {flag}"));
    }
    a
}

fn main() {
    let running = castcorder_host::start(parse_args(), |link| {
        println!();
        println!("  Guests anywhere open this call link (share it only with them):");
        println!("    {link}");
        println!();
        println!("CALL_LINK {link}");
    })
    .unwrap_or_else(|e| fail(&e));
    // Ctrl+C: finalise an in-progress take of the host microphone (WAV,
    // timing, manifest, FLAC) and finish the safety tracks before exiting;
    // what is not finished is recovered at the next start.
    let finisher = running.finisher();
    if let Err(e) = ctrlc::set_handler(move || {
        finisher.finish();
        std::process::exit(0);
    }) {
        tracing::warn!(error = %e, "Ctrl+C handler not installed; an interrupted take is recovered at the next start");
    }
    let links = running.lan_links();
    println!();
    println!("Castcorder host is running. Press Ctrl+C to stop.");
    println!();
    println!("  Call:      {}", running.call());
    println!("  Library:   {}", running.call_dir().display());
    println!("  Logs:      {}", running.call_dir().join("logs").display());
    println!();
    println!("  On THIS machine, open (producer controls, and join to be heard):");
    println!("    {}", running.producer_url());
    println!();
    println!("  Guests on the same network open (accept the certificate warning once):");
    for l in links {
        println!("    {l}");
    }
    println!();
    println!(
        "HOST_READY http=http://127.0.0.1:{} https={} udp={}",
        running.http_addr().port(),
        links
            .first()
            .map(|l| l.trim_end_matches('/'))
            .unwrap_or_default(),
        running.udp_addr()
    );
    loop {
        std::thread::park();
    }
}
