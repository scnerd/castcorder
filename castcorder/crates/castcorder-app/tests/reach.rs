//! The reachability self-check (castcorder-host `reach`, cast-qscw.5) across
//! the real boundaries: an app install holding its connection to a real Rust
//! rendezvous, checking that the rendezvous delivers to it as it would for
//! anyone else.
//!
//! - Registered, the install finds itself reachable.
//! - The rendezvous is redeployed (killed outright): while it is down the
//!   install does not claim to be reachable.
//! - Back on the same address, the install registers again and is reachable.

use std::net::SocketAddr;
use std::thread::sleep;
use std::time::{Duration, Instant};

use castcorder_host::Config;
use castcorder_rendezvous::mailbox::Limits;
use castcorder_rendezvous::server::{Config as RvConfig, bind};

fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(30);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        sleep(Duration::from_millis(50));
    }
}

/// The real rendezvous on a loopback port, on its own runtime, until
/// [`Rv::kill`] (as a redeploy does: every connection dropped at once).
struct Rv {
    addr: SocketAddr,
    stop: std::sync::mpsc::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

impl Rv {
    fn kill(self) {
        let _ = self.stop.send(());
        self.thread.join().expect("rendezvous thread");
    }
}

fn rendezvous_on(listen: SocketAddr) -> Rv {
    let (tx, rx) = std::sync::mpsc::channel();
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let bound = rt
            .block_on(bind(RvConfig {
                listen,
                stun: None,
                clients: vec![],
                limits: Limits::default(),
                public_host: None,
                trust_forwarded_for: false,
                turn: None,
                metering: None,
            }))
            .expect("bind");
        tx.send(bound.http).unwrap();
        rt.spawn(bound.run());
        let _ = stopped.recv();
        // Dropping the runtime drops every connection and the listener.
    });
    let addr = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("rendezvous up");
    Rv { addr, stop, thread }
}

#[test]
fn an_install_knows_whether_others_can_reach_it() {
    let root = std::env::temp_dir().join(format!("castcorder-app-reach-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let rv = rendezvous_on("127.0.0.1:0".parse().unwrap());
    let at = rv.addr;
    let dist = root.join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("index.html"), "<!doctype html>").unwrap();
    let mut c = Config::new(dist);
    c.library = root.join("a");
    c.room = "show".into();
    c.bind = std::net::Ipv4Addr::LOCALHOST.into();
    c.https_port = 0;
    c.http_port = 0;
    c.udp_port = 0;
    c.align = None;
    c.rendezvous = Some(format!("http://{at}"));
    let a = castcorder_host::start(c, |_| {}).unwrap();

    wait("the install to find itself reachable", || {
        a.reachable().0 == "yes"
    });

    rv.kill();
    wait("the install to stop claiming it is reachable", || {
        a.reachable().0.is_empty()
    });

    let rv = rendezvous_on(at);
    wait("the install reachable again", || a.reachable().0 == "yes");
    assert!(a.reachable().1.is_empty());

    a.stop();
    rv.kill();
    let _ = std::fs::remove_dir_all(&root);
}
