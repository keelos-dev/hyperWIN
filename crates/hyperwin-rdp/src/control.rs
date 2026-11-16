//! What the app on the PC (hyperwin-app) and the service say to each other: files in the state
//! folder, as hyperMAC's window and service do it (its settings.rs, status.json).
//!
//! - `settings.json`: the app's switches, e.g. `{"video": true, "sound": true, "clip": true,
//!   "paused": false}`. Read at each new connection, so a change applies from the next
//!   connection on, no restart. A HYPERWIN_VIDEO / _SOUND / _CLIP environment variable
//!   (development) wins over the file. Missing: on; "paused" missing: off. While "paused" is
//!   on the service turns new connections away; the ones up stay.
//!   "awake" (missing: on) is not about connections: while it is on and a VM is running the
//!   service keeps the PC from going to sleep by itself (hyperwin's awake.rs looks every few
//!   seconds).
//! - `status.json`: who is connected, written by the service whenever that changes.
//! - `disconnect`: a file the app makes to have the service end all connections; the service
//!   looks for it every second and removes it.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const PAUSED: &str = "paused";
pub const AWAKE: &str = "awake";

fn read(state: &Path) -> serde_json::Map<String, serde_json::Value> {
    std::fs::read(state.join("settings.json")).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

/// Is this feature on? `env` (e.g. "HYPERWIN_VIDEO") = "0" turns it off whatever the file says.
pub fn on(state: &Path, key: &str, env: &str) -> bool {
    if let Ok(v) = std::env::var(env) {
        return v != "0";
    }
    get(state, key, true)
}

/// A switch whose default is `default` (e.g. "paused", off unless set).
pub fn get(state: &Path, key: &str, default: bool) -> bool {
    read(state).get(key).and_then(serde_json::Value::as_bool).unwrap_or(default)
}

pub fn set(state: &Path, key: &str, value: bool) -> anyhow::Result<()> {
    let mut m = read(state);
    m.insert(key.into(), value.into());
    std::fs::create_dir_all(state)?;
    let tmp = state.join("settings.json.new");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&m)?)?;
    std::fs::rename(&tmp, state.join("settings.json"))?;
    Ok(())
}

/// One client, as status.json has it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    /// the client's address
    pub peer: String,
    /// the VM it looks at, once it has logged in
    pub vm: Option<String>,
}

static STATUS: OnceLock<PathBuf> = OnceLock::new();
static CONNECTIONS: Mutex<Vec<(u64, Connection)>> = Mutex::new(Vec::new());

fn write_status(connections: &[(u64, Connection)]) {
    let Some(path) = STATUS.get() else { return };
    let list: Vec<&Connection> = connections.iter().map(|(_, c)| c).collect();
    let tmp = path.with_extension("json.new");
    if std::fs::write(&tmp, serde_json::json!({ "connections": list }).to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// The service starts: nobody is connected, whatever an earlier run left in the file.
pub fn begin(state: &Path) {
    let _ = STATUS.set(state.join("status.json"));
    let _ = std::fs::remove_file(state.join("disconnect"));
    write_status(&CONNECTIONS.lock().unwrap());
}

/// A connection's entry in the status, there for as long as this lives.
pub struct Connected(u64);

impl Connected {
    pub fn new(peer: std::net::IpAddr) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut all = CONNECTIONS.lock().unwrap();
        all.push((id, Connection { peer: peer.to_canonical().to_string(), vm: None }));
        write_status(&all);
        Self(id)
    }

    pub fn set_vm(&self, vm: &str) {
        let mut all = CONNECTIONS.lock().unwrap();
        if let Some((_, c)) = all.iter_mut().find(|(id, _)| *id == self.0) {
            c.vm = Some(vm.to_owned());
        }
        write_status(&all);
    }
}

impl Drop for Connected {
    fn drop(&mut self) {
        let mut all = CONNECTIONS.lock().unwrap();
        all.retain(|(id, _)| *id != self.0);
        write_status(&all);
    }
}

/// The app's side: who is connected, as the service last wrote it. (Only worth anything while
/// the service runs: a service that was killed leaves its last list.)
pub fn connections(state: &Path) -> Vec<Connection> {
    #[derive(Deserialize)]
    struct Status {
        connections: Vec<Connection>,
    }
    std::fs::read(state.join("status.json")).ok().and_then(|b| serde_json::from_slice::<Status>(&b).ok()).map(|s| s.connections).unwrap_or_default()
}

/// The app's side: has the service end all connections (within a second).
pub fn ask_disconnect(state: &Path) -> std::io::Result<()> {
    std::fs::write(state.join("disconnect"), b"")
}

/// The service's side: was that asked since the last look?
pub fn disconnect_asked(state: &Path) -> bool {
    std::fs::remove_file(state.join("disconnect")).is_ok()
}

/// SHA-256 of the certificate RDP and the web interface use, as a client shows it at its first
/// connection: "AB:CD:...". None before the service has run once (it makes the certificate).
pub fn fingerprint(state: &Path) -> Option<String> {
    let pem = std::fs::read_to_string(state.join("tls").join("cert.pem")).ok()?;
    let hash = crate::l2::cert_sha256_of_pem(&pem)?;
    Some(hash.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches() {
        let state = std::env::temp_dir().join(format!("hyperwin-control-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        assert!(get(&state, "sound", true), "missing: the default");
        assert!(!get(&state, PAUSED, false));
        set(&state, "sound", false).unwrap();
        set(&state, PAUSED, true).unwrap();
        assert!(!get(&state, "sound", true));
        assert!(get(&state, PAUSED, false));
        assert!(get(&state, "clip", true), "the others stay");
        // asked once, seen once
        assert!(!disconnect_asked(&state));
        ask_disconnect(&state).unwrap();
        assert!(disconnect_asked(&state) && !disconnect_asked(&state));
        assert!(connections(&state).is_empty());
        let _ = std::fs::remove_dir_all(&state);
    }
}
