#!/bin/sh
# Builds a small RDP client for testing hyperwin's server without a GUI: IronRDP's "screenshot"
# example from vendor/IronRDP (it accepts any certificate), with
#   - the desktop size from RDP_W / RDP_H,
#   - RDP_NO_CREDSSP=1 for a TLS-only login (what kRDPclient does with a token),
#   - a read that times out counted as "the screen stands still" on Windows too.
# It logs in, draws what the server sends until nothing has come for 4 seconds, and saves a PNG.
# On a screen that keeps changing it never gets there: RDP_MAX_SECONDS=N saves after N seconds anyway.
# It takes sound too (PCM only): RDP_SOUND_OUT=FILE gets the samples as they arrive, raw.
#   sh tools/test/rdp-test-client.sh            (builds target/rdp-test-client/release/rdp-test-client.exe)
#   RDP_W=1280 RDP_H=800 rdp-test-client --host HOST --port 3390 -u admin@w10 -p PASSWORD -o out.png      NLA
#   RDP_NO_CREDSSP=1 ... -u w10 -p keel_...                                                               a token
set -e
D=$(cd "$(dirname "$0")/../.." && pwd)
sh "$D/tools/vendor-ironrdp.sh"
W="$D/target/rdp-test-client"
mkdir -p "$W/src"
V=$(cd "$D/vendor/IronRDP/crates" && (pwd -W 2>/dev/null || pwd))
sed -e 's/width: 1280,/width: std::env::var("RDP_W").ok().and_then(|v| v.parse().ok()).unwrap_or(1280),/' \
    -e 's/height: 1024,/height: std::env::var("RDP_H").ok().and_then(|v| v.parse().ok()).unwrap_or(800),/' \
    -e 's/enable_credssp: true,/enable_credssp: std::env::var_os("RDP_NO_CREDSSP").is_none(),/' \
    -e 's/enable_tls: false,/enable_tls: true,/' \
    -e 's/Duration::from_secs(3)/Duration::from_secs(4)/' \
    -e "s/    'outer: loop {/    let started = std::time::Instant::now(); let most = std::env::var(\"RDP_MAX_SECONDS\").ok().and_then(|v| v.parse().ok()).unwrap_or(u64::MAX); 'outer: loop { if started.elapsed().as_secs() >= most { break 'outer; }/" \
    -e 's/Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break/Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => break/' \
    -e 's/connector::ClientConnector::new(config, client_addr);/connector::ClientConnector::new(config, client_addr).with_static_channel(ironrdp::rdpsnd::client::Rdpsnd::new(Box::new(Waves::new())));/' \
    "$V/ironrdp/examples/screenshot.rs" > "$W/src/main.rs"
# sound: the waves to a file (rdp-test-sound.rs)
cat "$D/tools/test/rdp-test-sound.rs" >> "$W/src/main.rs"
cat > "$W/Cargo.toml" <<TOML
[package]
name = "rdp-test-client"
version = "0.0.0"
edition = "2024"
publish = false

[workspace]

[dependencies]
ironrdp = { path = "$V/ironrdp", features = ["session", "connector", "graphics", "rdpsnd"] }
ironrdp-blocking = { path = "$V/ironrdp-blocking" }
ironrdp-pdu = { path = "$V/ironrdp-pdu" }
ironrdp-graphics = { path = "$V/ironrdp-graphics" }
anyhow = "1"
image = { version = "0.25", default-features = false, features = ["png"] }
pico-args = "0.5"
x509-cert = { version = "0.3", default-features = false, features = ["std"] }
sspi = { version = "0.21", features = ["network_client"] }
tracing = { version = "0.1", features = ["log"] }
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
tokio-rustls = "0.26"
TOML
cd "$W" && cargo build --release --target-dir "$W"
echo "$W/release/rdp-test-client.exe"
