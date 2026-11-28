//! Relay (TURN) credentials are minted only for a host key that is online:
//! the real server on a loopback port, real WebSocket clients (cast-dns).
//!
//! - A guest naming a host that is not connected gets no `turn:` server, and
//!   the hello's `relay_error` says the host is offline.
//! - A host gets its own credentials in `registered`, after proving its key.
//! - While it is registered, a guest naming it gets credentials; once it has
//!   gone, no more.
//! - A host key over its relay budget (a stored ledger) gets no credentials,
//!   as a host or for its guests (`relay_quota`); the admin listener reports
//!   its usage. One near it (90% or more) gets short-lived credentials.

use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use castcorder_link::wire::{
    ClientMsg, IceServer, ServerMsg, b64, code, from_json, to_json, unb64,
};
use castcorder_link::{CallSecret, HostKey};
use castcorder_rendezvous::mailbox::Limits;
use castcorder_rendezvous::server::{Config, Metering, bind};
use castcorder_rendezvous::turn::TurnConfig;
use castcorder_rendezvous::usage::{LEDGER_FILE, Ledger, save};
use tungstenite::{Message, WebSocket};

type Ws = WebSocket<TcpStream>;

/// Start the real server on an OS-picked loopback port, on its own runtime.
fn start() -> SocketAddr {
    start_with(None).0
}

/// The same with relay metering; also returns the admin address.
fn start_with(metering: Option<Metering>) -> (SocketAddr, Option<SocketAddr>) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async move {
            let bound = bind(Config {
                listen: "127.0.0.1:0".parse().unwrap(),
                stun: None,
                clients: vec![],
                limits: Limits::default(),
                public_host: None,
                trust_forwarded_for: false,
                no_ping: false,
                turn: Some(TurnConfig {
                    urls: vec!["turn:turn.test:3478?transport=udp".into()],
                    secret: "test-secret".into(),
                    ttl: Duration::from_secs(600),
                }),
                metering,
            })
            .await
            .expect("bind");
            tx.send((bound.http, bound.admin)).unwrap();
            bound.run().await.expect("serve");
        });
    });
    rx.recv_timeout(Duration::from_secs(10)).expect("server up")
}

/// The call secret the test hosts admit (their registered gate), and the
/// guests' proof of it.
fn secret() -> CallSecret {
    CallSecret([3; 16])
}

/// A connection naming `host`, with the proof of the test call link.
fn connect(addr: SocketAddr, host: &str) -> Ws {
    connect_with(addr, host, Some(&secret().relay_proof()))
}

fn connect_with(addr: SocketAddr, host: &str, proof: Option<&str>) -> Ws {
    let stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let url = match proof {
        Some(p) => format!("ws://{addr}/ws?host={host}&proof={p}"),
        None => format!("ws://{addr}/ws?host={host}"),
    };
    tungstenite::client(url.as_str(), stream)
        .expect("handshake")
        .0
}

fn read(ws: &mut Ws) -> ServerMsg {
    loop {
        match ws.read().expect("read") {
            Message::Text(t) => return from_json(t.as_str()).expect("a server frame"),
            _ => continue,
        }
    }
}

fn hello(ws: &mut Ws) -> (String, Vec<IceServer>, Option<String>) {
    match read(ws) {
        ServerMsg::Hello {
            nonce,
            ice_servers,
            relay_error,
            ..
        } => (nonce, ice_servers, relay_error),
        other => panic!("expected hello, got {other:?}"),
    }
}

fn turn_users(servers: &[IceServer]) -> Vec<String> {
    servers
        .iter()
        .filter(|s| s.urls.iter().any(|u| u.starts_with("turn:")))
        .filter_map(|s| s.username.clone())
        .collect()
}

#[test]
fn relay_credentials_only_for_a_host_that_is_online() {
    let addr = start();
    let key = HostKey::from_secret([7; 32]);
    let id = key.id().encode();

    // Not online: nothing minted, and the hello says why.
    let mut early = connect(addr, &id);
    let (_, ice, why) = hello(&mut early);
    assert!(turn_users(&ice).is_empty(), "no credentials: {ice:?}");
    let why = why.expect("a relay error");
    assert!(why.starts_with(code::HOST_OFFLINE), "{why}");
    drop(early);

    // The host registers and gets its own credentials in `registered`.
    let mut host = connect(addr, &id);
    let (nonce, _, _) = hello(&mut host);
    let sig = key.sign_register(&unb64(&nonce).unwrap());
    let reg = ClientMsg::Register {
        host: id.clone(),
        sig: b64(&sig),
    };
    host.send(Message::Text(to_json(&reg).into())).unwrap();
    match read(&mut host) {
        ServerMsg::Registered { ice_servers, .. } => {
            let users = turn_users(&ice_servers);
            assert_eq!(users.len(), 1, "{ice_servers:?}");
            assert!(users[0].ends_with(&format!(":{id}")), "{}", users[0]);
        }
        other => panic!("expected registered, got {other:?}"),
    }
    send_gates(&mut host, &[secret()]);
    std::thread::sleep(Duration::from_millis(200));

    // Online: a guest naming it gets credentials.
    let mut guest = connect(addr, &id);
    let (_, ice, why) = hello(&mut guest);
    assert_eq!(turn_users(&ice).len(), 1, "{ice:?}");
    assert_eq!(why, None);
    drop(guest);

    // Online, but a stranger who knows only the host id (no proof, or the
    // proof of some other secret) gets nothing (cast-d74).
    for proof in [None, Some(CallSecret([4; 16]).relay_proof())] {
        let (_, ice, why) = hello(&mut connect_with(addr, &id, proof.as_deref()));
        assert!(turn_users(&ice).is_empty(), "{proof:?}: {ice:?}");
        let why = why.expect("a relay error");
        assert!(why.starts_with(code::RELAY_PROOF), "{why}");
    }
    // The host regenerates its link: the old proof stops working, the new one works.
    send_gates(&mut host, &[CallSecret([9; 16])]);
    let t0 = Instant::now();
    loop {
        let (_, ice, _) = hello(&mut connect(addr, &id));
        if turn_users(&ice).is_empty() {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "the old proof still works"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let new = CallSecret([9; 16]).relay_proof();
    let (_, ice, why) = hello(&mut connect_with(addr, &id, Some(&new)));
    assert_eq!(turn_users(&ice).len(), 1, "{ice:?}: {why:?}");
    send_gates(&mut host, &[secret()]);
    let t0 = Instant::now();
    while turn_users(&hello(&mut connect(addr, &id)).1).is_empty() {
        assert!(t0.elapsed() < Duration::from_secs(10), "gates not restored");
        std::thread::sleep(Duration::from_millis(50));
    }

    // Another key that never connected: nothing.
    let other = HostKey::from_secret([8; 32]).id().encode();
    let (_, ice, why) = hello(&mut connect(addr, &other));
    assert!(turn_users(&ice).is_empty() && why.is_some());

    // Gone: nothing again (the server notices the close).
    host.close(None).unwrap();
    while host.read().is_ok() {}
    let t0 = Instant::now();
    loop {
        let (_, ice, why) = hello(&mut connect(addr, &id));
        if turn_users(&ice).is_empty() {
            assert!(why.unwrap().starts_with(code::HOST_OFFLINE));
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "still minting after the host left"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A registered host's current relay gates, one per call secret.
fn send_gates(ws: &mut Ws, secrets: &[CallSecret]) {
    let m = ClientMsg::RelayGates {
        gates: secrets.iter().map(CallSecret::relay_gate).collect(),
    };
    ws.send(Message::Text(to_json(&m).into())).unwrap();
}

fn register(addr: SocketAddr, key: &HostKey) -> (Ws, Vec<IceServer>, Option<String>) {
    let id = key.id().encode();
    let mut ws = connect(addr, &id);
    let (nonce, _, _) = hello(&mut ws);
    let reg = ClientMsg::Register {
        host: id,
        sig: b64(&key.sign_register(&unb64(&nonce).unwrap())),
    };
    ws.send(Message::Text(to_json(&reg).into())).unwrap();
    match read(&mut ws) {
        ServerMsg::Registered {
            ice_servers,
            relay_error,
            ..
        } => {
            send_gates(&mut ws, &[secret()]);
            // The server handles the frame on its own task; give it a moment.
            std::thread::sleep(Duration::from_millis(200));
            (ws, ice_servers, relay_error)
        }
        other => panic!("expected registered, got {other:?}"),
    }
}

fn http_get(addr: SocketAddr, path: &str) -> String {
    use std::io::{Read, Write};
    let mut s = TcpStream::connect(addr).expect("connect admin");
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(s, "GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").unwrap();
    let mut body = String::new();
    s.read_to_string(&mut body).unwrap();
    body
}

#[test]
fn a_host_key_over_its_relay_budget_gets_no_relay_credentials() {
    let over = HostKey::from_secret([21; 32]);
    let under = HostKey::from_secret([22; 32]);
    let near = HostKey::from_secret([23; 32]);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // A ledger left by an earlier run: one host key over the budget.
    let dir = std::env::temp_dir().join(format!("cc-rv-quota-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut ledger = Ledger::default();
    ledger.record(&over.id(), 5_000, now);
    ledger.record(&under.id(), 10, now);
    ledger.record(&near.id(), 950, now);
    save(&dir.join(LEDGER_FILE), &ledger.to_json()).unwrap();

    let (addr, admin) = start_with(Some(Metering {
        metrics_url: None,
        interval: Duration::from_secs(30),
        data_dir: Some(dir.clone()),
        budget: Some(1_000),
        admin: Some("127.0.0.1:0".parse().unwrap()),
    }));

    // Over budget: registered, but no relay, and the frame says why.
    let (_over_ws, ice, why) = register(addr, &over);
    assert!(turn_users(&ice).is_empty(), "{ice:?}");
    let why = why.expect("a relay error");
    assert!(why.starts_with(code::RELAY_QUOTA), "{why}");
    // A guest calling it: the same in the hello.
    let (_, ice, why) = hello(&mut connect(addr, &over.id().encode()));
    assert!(turn_users(&ice).is_empty(), "{ice:?}");
    assert!(why.unwrap().starts_with(code::RELAY_QUOTA));

    // Under budget: minted as usual.
    let (_under_ws, ice, why) = register(addr, &under);
    assert_eq!(turn_users(&ice).len(), 1, "{ice:?}");
    assert_eq!(why, None);
    let (_, ice, why) = hello(&mut connect(addr, &under.id().encode()));
    assert_eq!(turn_users(&ice).len(), 1);
    assert_eq!(why, None);
    let expiry =
        |users: Vec<String>| -> u64 { users[0].split(':').next().unwrap().parse().unwrap() };
    assert!(expiry(turn_users(&ice)) >= now + 600, "{ice:?}");

    // Near it: minted, but for five minutes (the TTL here is ten), so
    // coturn reports the sessions' bytes sooner.
    let (_near_ws, ice, why) = register(addr, &near);
    assert_eq!(why, None);
    let e = expiry(turn_users(&ice));
    assert!(e >= now + 300 && e <= now + 300 + 150 + 5, "{e} vs {now}");
    let (_, ice, _) = hello(&mut connect(addr, &near.id().encode()));
    let e = expiry(turn_users(&ice));
    assert!(e >= now + 300 && e <= now + 300 + 150 + 5, "{e} vs {now}");

    // The operator's view on the admin listener.
    let resp = http_get(admin.expect("admin bound"), "/usage");
    assert!(
        resp.starts_with("HTTP/1.0 200") || resp.starts_with("HTTP/1.1 200"),
        "{resp}"
    );
    let body = resp.split_once("\r\n\r\n").unwrap().1;
    assert!(
        body.contains(&format!(
            "{{\"host\":\"{}\",\"bytes\":5000,\"over_budget\":true}}",
            over.id().encode()
        )),
        "{body}"
    );
    assert!(body.contains("\"budget_bytes\":1000"), "{body}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_client_that_keeps_reconnecting_reuses_its_credential_and_counts_once() {
    let addr = start();
    let key = HostKey::from_secret([31; 32]);
    let (_host_ws, _, _) = register(addr, &key);
    let id = key.id().encode();

    // More hellos from one address than the host key's bucket (burst 30)
    // holds: each is a client re-asking, and each gets the same credential.
    let mut first = None;
    for n in 0..45 {
        let (_, ice, why) = hello(&mut connect(addr, &id));
        assert_eq!(why, None, "hello {n}");
        let users = turn_users(&ice);
        assert_eq!(users.len(), 1, "hello {n}: {ice:?}");
        assert_eq!(first.get_or_insert_with(|| users[0].clone()), &users[0]);
    }
}
