//! NOTE: with the `ssr` feature on, calling `add(1, 2)` directly runs the body
//! in-process (that is how SSR works). To force the wire path in a single
//! test binary we call the generated request structs' `run_on_client()`,
//! which is exactly what the non-ssr stub does.

use futures::StreamExt;
use server_fn::ServerFn;
use sfspike::api::{Add, Device, Fail, ListDevices, StartMeters};
use sfspike::channel::duplex;
use sfspike::client::Connection;
use sfspike::host::{registered_paths, Peer, DISPATCHED};
use std::sync::atomic::Ordering;

#[tokio::test(flavor = "multi_thread")]
async fn server_fns_round_trip_over_in_memory_channel() {
    let (client_end, host_end) = duplex();

    // Host side: Peer gets every message the "data channel" delivers.
    let peer = Peer::new(host_end.tx.clone());
    let mut host_rx = host_end.rx;
    tokio::spawn(async move {
        while let Some(m) = host_rx.next().await {
            peer.on_message(m);
        }
    });

    // Client side: Connection writes to the channel; pump feeds replies back.
    let conn = Connection::new(client_end.tx.clone());
    conn.install().unwrap();
    let mut events = conn.events();
    let mut client_rx = client_end.rx;
    let pump_conn = conn.clone();
    tokio::spawn(async move {
        while let Some(m) = client_rx.next().await {
            pump_conn.on_message(&m);
        }
    });

    println!("registered: {:?}", registered_paths());
    assert_eq!(registered_paths().len(), 4);

    assert_eq!(Add { a: 2, b: 40 }.run_on_client().await.unwrap(), 42);
    assert_eq!(DISPATCHED.load(Ordering::SeqCst), 1, "call must cross the channel");

    assert_eq!(
        ListDevices { prefix: "Mic".into() }.run_on_client().await.unwrap(),
        vec![
            Device { id: 0, name: "Mic A".into() },
            Device { id: 1, name: "Mic B".into() }
        ]
    );

    // Typed error crosses the channel.
    let err = Fail { msg: "nope".into() }.run_on_client().await.unwrap_err();
    assert!(err.to_string().contains("nope"), "got: {err}");

    // Concurrent calls are multiplexed by request id.
    let (a, b, c) = tokio::join!(
        Add { a: 1, b: 1 }.run_on_client(),
        Add { a: 2, b: 2 }.run_on_client(),
        Add { a: 3, b: 3 }.run_on_client()
    );
    assert_eq!((a.unwrap(), b.unwrap(), c.unwrap()), (2, 4, 6));

    // Server push: the call returns, events arrive on the side channel, and
    // the body knew which peer called (task-local set by the dispatcher).
    StartMeters { n: 3 }.run_on_client().await.unwrap();
    for i in 0..3 {
        let ev = events.next().await.unwrap();
        assert_eq!(ev.topic, "meter");
        let v: f32 = postcard::from_bytes(&ev.body).unwrap();
        assert!((v - i as f32 * 0.1).abs() < 1e-6);
    }
    assert_eq!(DISPATCHED.load(Ordering::SeqCst), 7);
}
