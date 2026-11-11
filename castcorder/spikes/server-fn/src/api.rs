//! The shared API. Written once; compiled with `ssr` on the host (bodies are
//! real and registered) and without on the client (bodies are stripped, the
//! same names become async stubs that go through `HostClient`).

use crate::client::HostClient;
use serde::{Deserialize, Serialize};
use server_fn::codec::Postcard;
use server_fn::ServerFnError;
use server_fn_macro_default::server;

#[cfg(feature = "ssr")]
use crate::host::HostServer;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub id: u32,
    pub name: String,
}

#[server(client = HostClient, server = HostServer, input = Postcard, output = Postcard)]
pub async fn add(a: i32, b: i32) -> Result<i32, ServerFnError> {
    Ok(a + b)
}

#[server(client = HostClient, server = HostServer, input = Postcard, output = Postcard)]
pub async fn list_devices(prefix: String) -> Result<Vec<Device>, ServerFnError> {
    let all = ["Mic A", "Mic B", "Webcam"];
    Ok(all
        .iter()
        .enumerate()
        .filter(|(_, n)| n.starts_with(&prefix))
        .map(|(i, n)| Device {
            id: i as u32,
            name: n.to_string(),
        })
        .collect())
}

#[server(client = HostClient, server = HostServer, input = Postcard, output = Postcard)]
pub async fn fail(msg: String) -> Result<(), ServerFnError> {
    Err(ServerFnError::new(msg))
}

/// Asks the host to start pushing `n` meter events to *this* peer.
#[server(client = HostClient, server = HostServer, input = Postcard, output = Postcard)]
pub async fn start_meters(n: u32) -> Result<(), ServerFnError> {
    let peer = crate::host::current_peer();
    for i in 0..n {
        peer.push_event("meter", &(i as f32 * 0.1))
            .map_err(ServerFnError::new)?;
    }
    Ok(())
}
