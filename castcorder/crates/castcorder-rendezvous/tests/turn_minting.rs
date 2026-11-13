//! Relay (TURN) credentials are minted only for a host key that is online:
//! the real server on a loopback port, real WebSocket clients (cast-dns).
//!
//! - A guest naming a host that is not connected gets no `turn:` server, and
//!   the hello's `relay_error` says the host is offline.
//! - A host gets its own credentials in `registered`, after proving its key.
//! - While it is registered, a guest naming it gets credentials; once it has
//!   gone, no more.

use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use castcorder_link::HostKey;
use castcorder_link::wire::{
    ClientMsg, IceServer, ServerMsg, b64, code, from_json, to_json, unb64,
};
use castcorder_rendezvous::mailbox::Limits;
use castcorder_rendezvous::server::{Config, bind};
use castcorder_rendezvous::turn::TurnConfig;
use tungstenite::{Message, WebSocket};

type Ws = WebSocket<TcpStream>;

/// Start the real server on an OS-picked loopback port, on its own runtime.
fn start() -> SocketAddr {
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
                turn: Some(TurnConfig {
                    urls: vec!["turn:turn.test:3478?transport=udp".into()],
                    secret: "test-secret".into(),
                    ttl: Duration::from_secs(600),
                }),
            })
            .await
            .expect("bind");
            tx.send(bound.http).unwrap();
            bound.run().await.expect("serve");
        });
    });
    rx.recv_timeout(Duration::from_secs(10)).expect("server up")
}

fn connect(addr: SocketAddr, host: &str) -> Ws {
    let stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let url = format!("ws://{addr}/ws?host={host}");
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

    // Online: a guest naming it gets credentials.
    let mut guest = connect(addr, &id);
    let (_, ice, why) = hello(&mut guest);
    assert_eq!(turn_users(&ice).len(), 1, "{ice:?}");
    assert_eq!(why, None);
    drop(guest);

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
