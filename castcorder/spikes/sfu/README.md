# SFU spike: str0m as host transport and forwarding unit

Throwaway spike. The findings (verdict and numbers) were reported to the coordinator; FINDINGS.md was not written because the harness refused report files.

## Run the automated test (one command)

First time only:

    npm install
    npm run setup        # downloads Chromium into node_modules (no global install)

Then:

    npm test

`npm test` builds `target/release/host` (cargo, release), launches it, opens 3
headless Chromium guests with fake camera/mic, and checks forwarding,
data channel echo, a 50 MB blob hash, graceful leave/rejoin and a killed-page
leave/rejoin. Exit code 0 means every check passed. Output:
`test-results/summary.json` and `test-results/host.log`.

Environment knobs (all optional):

| Variable | Effect |
|---|---|
| CC_VIDEO_CODEC=h264 or vp8 | host offers only Opus + that video codec |
| CC_BWE_KBPS=off or N | disable BWE or set its initial estimate (default 1500) |
| CC_FAST_ICE_TIMEOUT=1 | shorter STUN RTO/retransmits (faster dead-peer detection) |
| CC_FEATURES=rust-crypto (or wincrypto) | build the host with another crypto backend |
| CC_HOST_EXE=path | test an already built host binary |
| CC_SKIP_BUILD=1 | do not run cargo build |
| CC_BLOB_MB=N | blob size (default 50) |
| CC_MIX=chromium,firefox,chromium | browser engine per guest (Firefox crashed; see FINDINGS) |

## Manual use

    cargo run --release --bin host

It prints `HOST_READY http=http://127.0.0.1:PORT ...`. Open
`http://127.0.0.1:PORT/?name=A&auto=1` in a few Chrome tabs/windows
(localhost is a secure context, so getUserMedia works over plain HTTP).
UDP is bound to 127.0.0.1 by default; set `CC_UDP_IP` to a LAN address to
use other machines or Firefox.

## Layout

- `src/main.rs`  HTTP (static page, POST /offer, GET /stats), Rtc construction
- `src/sfu.rs`   single-threaded forwarding loop over one UDP socket
- `web/guest.html` the guest (plain JS), exposes `window.cc` for the test
- `test/e2e.mjs` the Playwright test
