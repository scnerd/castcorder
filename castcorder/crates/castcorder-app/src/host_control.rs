//! The host the app runs in its own process: started with the app, kept
//! running while the window is hidden in the tray, stopped (its open files
//! finished) when the app quits.

use std::net::SocketAddr;
use std::sync::Mutex;

use castcorder_host::{Config, Running};

/// What the window and the tray show about the running host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostStatus {
    /// The call's name.
    pub call: String,
    /// The producer page the window shows (`http://localhost:<port>/`).
    pub producer_url: String,
    /// The producer page's socket address.
    pub http_addr: SocketAddr,
    /// The LAN join links.
    pub lan_links: Vec<String>,
    /// The call's UDP address.
    pub udp_addr: SocketAddr,
    /// The call link for guests anywhere, once registered with a rendezvous.
    pub call_link: Option<String>,
}

/// Starts and stops the in-process host for one [`Config`].
pub struct HostControl {
    config: Config,
    running: Mutex<Option<Running>>,
}

impl HostControl {
    /// A control for `config`'s host; nothing runs until [`HostControl::start`].
    pub fn new(config: Config) -> Self {
        HostControl {
            config,
            running: Mutex::new(None),
        }
    }

    /// The config the host starts with.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Start the host, or report the one already running.
    pub fn start(&self) -> Result<HostStatus, String> {
        let mut running = self.running.lock().map_err(|_| "host lock poisoned")?;
        if running.is_none() {
            let call = self.config.call.clone();
            let r = castcorder_host::start(self.config.clone(), move |link| {
                tracing::info!(call = %call, link = %link, "call link ready");
            })?;
            *running = Some(r);
        }
        Ok(status(running.as_ref().expect("just started")))
    }

    /// The running host, if any.
    pub fn status(&self) -> Option<HostStatus> {
        self.running.lock().ok()?.as_ref().map(status)
    }

    /// Finish the host's open files and stop it; nothing happens if it is not
    /// running. Its ports are free when this returns.
    pub fn stop(&self) {
        let running = self.running.lock().ok().and_then(|mut r| r.take());
        if let Some(r) = running {
            r.stop();
        }
    }
}

impl Drop for HostControl {
    fn drop(&mut self) {
        self.stop();
    }
}

fn status(r: &Running) -> HostStatus {
    HostStatus {
        call: r.call().to_string(),
        producer_url: r.producer_url(),
        http_addr: r.http_addr(),
        lan_links: r.lan_links().to_vec(),
        udp_addr: r.udp_addr(),
        call_link: r.call_link(),
    }
}
