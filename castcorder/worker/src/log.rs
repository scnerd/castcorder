//! `tracing` events (the shared mailbox logs through it) to the Workers
//! console, one line per event, as `wrangler dev` and Workers Logs show them.

use std::io::Write;

/// Collects one formatted event; logs it when dropped.
struct Line(Vec<u8>);

impl Write for Line {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        let text = String::from_utf8_lossy(&self.0);
        let text = text.trim_end();
        if !text.is_empty() {
            worker::console_log!("{text}");
        }
    }
}

/// Install the console subscriber (once per isolate).
pub fn init() {
    let _ = tracing_subscriber::fmt()
        .with_writer(|| Line(Vec::new()))
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .with_max_level(tracing::Level::INFO)
        .try_init();
}
