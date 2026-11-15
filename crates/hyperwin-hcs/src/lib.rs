//! The Host Compute System (computecore.dll) and Host Compute Network (computenetwork.dll) APIs.
//!
//! These are what the "Virtual Machine Platform" Windows feature gives: virtual machines without
//! Hyper-V's management layer (docs/notes/2026-11-14-v1-hcs-vmp-only.md). Everything goes in and
//! comes out as JSON documents (the HCS schema); this crate does the calls, the strings and the
//! errors, and leaves the documents to its callers.
//!
//! A compute system belongs to HCS, not to the process that made it: it keeps running when this
//! process ends, as long as its configuration does not set `ShouldTerminateOnLastHandleClosed`.
//! So every call here opens the system by id, does one thing and closes it again.
//!
//! The calls need an elevated process.

#![cfg(windows)]

pub mod hdv;

use std::ffi::c_void;
use std::fmt;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

type Hresult = i32;
type Handle = *mut c_void;

#[repr(C)]
struct Guid(u32, u16, u16, [u8; 8]);

#[link(name = "computecore", kind = "raw-dylib")]
extern "system" {
    fn HcsCreateOperation(context: *const c_void, callback: *const c_void) -> Handle;
    fn HcsCloseOperation(op: Handle);
    fn HcsWaitForOperationResult(op: Handle, timeout_ms: u32, result: *mut *mut u16) -> Hresult;
    fn HcsCreateComputeSystem(id: *const u16, config: *const u16, op: Handle, sd: *const c_void, system: *mut Handle) -> Hresult;
    fn HcsOpenComputeSystem(id: *const u16, access: u32, system: *mut Handle) -> Hresult;
    fn HcsCloseComputeSystem(system: Handle);
    fn HcsStartComputeSystem(system: Handle, op: Handle, options: *const u16) -> Hresult;
    fn HcsShutDownComputeSystem(system: Handle, op: Handle, options: *const u16) -> Hresult;
    fn HcsTerminateComputeSystem(system: Handle, op: Handle, options: *const u16) -> Hresult;
    fn HcsGetComputeSystemProperties(system: Handle, op: Handle, query: *const u16) -> Hresult;
    fn HcsModifyComputeSystem(system: Handle, op: Handle, config: *const u16, identity: Handle) -> Hresult;
    fn HcsEnumerateComputeSystems(query: *const u16, op: Handle) -> Hresult;
    fn HcsGrantVmAccess(vm_id: *const u16, file: *const u16) -> Hresult;
}

// HCN's strings come from CoTaskMemAlloc, HCS's from LocalAlloc.
#[link(name = "computenetwork", kind = "raw-dylib")]
extern "system" {
    fn HcnEnumerateNetworks(query: *const u16, networks: *mut *mut u16, error: *mut *mut u16) -> Hresult;
    fn HcnCreateNetwork(id: *const Guid, settings: *const u16, network: *mut Handle, error: *mut *mut u16) -> Hresult;
    fn HcnOpenNetwork(id: *const Guid, network: *mut Handle, error: *mut *mut u16) -> Hresult;
    fn HcnQueryNetworkProperties(network: Handle, query: *const u16, properties: *mut *mut u16, error: *mut *mut u16) -> Hresult;
    fn HcnCloseNetwork(network: Handle) -> Hresult;
    fn HcnDeleteNetwork(id: *const Guid, error: *mut *mut u16) -> Hresult;
    fn HcnCreateEndpoint(network: Handle, id: *const Guid, settings: *const u16, endpoint: *mut Handle, error: *mut *mut u16) -> Hresult;
    fn HcnQueryEndpointProperties(endpoint: Handle, query: *const u16, properties: *mut *mut u16, error: *mut *mut u16) -> Hresult;
    fn HcnCloseEndpoint(endpoint: Handle) -> Hresult;
    fn HcnDeleteEndpoint(id: *const Guid, error: *mut *mut u16) -> Hresult;
}

#[link(name = "kernel32")]
extern "system" {
    fn LocalFree(p: *mut c_void) -> *mut c_void;
}

#[link(name = "ole32")]
extern "system" {
    fn CoTaskMemFree(p: *mut c_void);
}

const GENERIC_ALL: u32 = 0x1000_0000;

/// How long one HCS operation may take. Creating and starting a VM answer within seconds; a
/// guest shutdown is given its own, longer wait by the caller.
const OPERATION_MS: u32 = 120_000;

/// An HCS or HCN call that failed: which call, its HRESULT, and the error document the API gave
/// (JSON, often with a readable `ErrorMessage`).
#[derive(Debug)]
pub struct Error {
    pub call: &'static str,
    pub hresult: u32,
    pub document: String,
}

impl Error {
    /// HCS_E_SYSTEM_NOT_FOUND: no compute system with that id.
    pub fn is_not_found(&self) -> bool {
        self.hresult == 0x8037_010E
    }

    /// HCN_E_NETWORK_ALREADY_EXISTS / HCN_E_ENDPOINT_ALREADY_EXISTS.
    pub fn already_exists(&self) -> bool {
        matches!(self.hresult, 0x803B_0010 | 0x803B_0014)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: 0x{:08X}", self.call, self.hresult)?;
        if !self.document.is_empty() {
            write!(f, " {}", self.document)?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

unsafe fn take(p: *mut u16, free: unsafe fn(*mut u16)) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
    free(p);
    s
}

unsafe fn free_local(p: *mut u16) {
    LocalFree(p.cast());
}

unsafe fn free_co(p: *mut u16) {
    CoTaskMemFree(p.cast());
}

fn check(call: &'static str, hr: Hresult, document: String) -> Result<()> {
    if hr >= 0 {
        Ok(())
    } else {
        Err(Error { call, hresult: hr as u32, document })
    }
}

/// One HCS call that takes an operation: run it, wait for it, give back its result document.
fn operation(call: &'static str, timeout_ms: u32, f: impl FnOnce(Handle) -> Hresult) -> Result<String> {
    // SAFETY: the operation handle is created, used and closed here; the result string is HCS's
    // to allocate and ours to free.
    unsafe {
        let op = HcsCreateOperation(std::ptr::null(), std::ptr::null());
        if op.is_null() {
            return Err(Error { call: "HcsCreateOperation", hresult: 0x8007_000E, document: String::new() });
        }
        let hr = f(op);
        if hr < 0 {
            HcsCloseOperation(op);
            return check(call, hr, String::new()).map(|()| String::new());
        }
        let mut result = std::ptr::null_mut();
        let hr = HcsWaitForOperationResult(op, timeout_ms, &mut result);
        let document = take(result, free_local);
        HcsCloseOperation(op);
        if hr < 0 {
            return Err(Error { call, hresult: hr as u32, document });
        }
        Ok(document)
    }
}

struct System(Handle);

impl System {
    fn open(id: &str) -> Result<Self> {
        let mut h = std::ptr::null_mut();
        // SAFETY: id is a NUL-terminated wide string that outlives the call.
        let hr = unsafe { HcsOpenComputeSystem(wide(id).as_ptr(), GENERIC_ALL, &mut h) };
        check("HcsOpenComputeSystem", hr, String::new())?;
        Ok(Self(h))
    }
}

impl Drop for System {
    fn drop(&mut self) {
        // SAFETY: the handle came from HcsOpenComputeSystem / HcsCreateComputeSystem.
        unsafe { HcsCloseComputeSystem(self.0) };
    }
}

/// The compute systems HCS knows, as its JSON array (`Id`, `SystemType`, `Owner`, `State`...).
pub fn enumerate() -> Result<String> {
    // SAFETY: a null query asks for everything.
    operation("HcsEnumerateComputeSystems", OPERATION_MS, |op| unsafe { HcsEnumerateComputeSystems(std::ptr::null(), op) })
}

/// Creates a compute system from an HCS configuration document. It is not started.
pub fn create(id: &str, configuration: &str) -> Result<()> {
    let (id, configuration) = (wide(id), wide(configuration));
    let mut h = std::ptr::null_mut();
    let r = operation("HcsCreateComputeSystem", OPERATION_MS, |op| unsafe {
        // SAFETY: both strings are NUL-terminated and outlive the call; no security descriptor.
        HcsCreateComputeSystem(id.as_ptr(), configuration.as_ptr(), op, std::ptr::null(), &mut h)
    });
    if !h.is_null() {
        drop(System(h));
    }
    r.map(|_| ())
}

pub fn start(id: &str) -> Result<()> {
    let system = System::open(id)?;
    // SAFETY: the handle is open for the duration of the call.
    operation("HcsStartComputeSystem", OPERATION_MS, |op| unsafe { HcsStartComputeSystem(system.0, op, std::ptr::null()) }).map(|_| ())
}

/// Asks the guest to shut down and waits up to `timeout_ms` for it. `options` is HCS's
/// shutdown options document (how the guest is asked: `Mechanism`, `Type`), or None for HCS's
/// default, which a VM (as opposed to a container) does not support.
pub fn shut_down(id: &str, options: Option<&str>, timeout_ms: u32) -> Result<()> {
    let system = System::open(id)?;
    let options = options.map(wide);
    let ptr = options.as_ref().map_or(std::ptr::null(), |o| o.as_ptr());
    // SAFETY: as in start; ptr is null or a NUL-terminated string that outlives the call.
    operation("HcsShutDownComputeSystem", timeout_ms, |op| unsafe { HcsShutDownComputeSystem(system.0, op, ptr) }).map(|_| ())
}

/// Powers the VM off: no shutdown in the guest.
pub fn terminate(id: &str) -> Result<()> {
    let system = System::open(id)?;
    // SAFETY: as in start.
    operation("HcsTerminateComputeSystem", OPERATION_MS, |op| unsafe { HcsTerminateComputeSystem(system.0, op, std::ptr::null()) }).map(|_| ())
}

/// The system's properties as JSON; `query` is an HCS property query document or None.
pub fn properties(id: &str, query: Option<&str>) -> Result<String> {
    let system = System::open(id)?;
    let query = query.map(wide);
    let ptr = query.as_ref().map_or(std::ptr::null(), |q| q.as_ptr());
    // SAFETY: ptr is null or a NUL-terminated string that outlives the call.
    operation("HcsGetComputeSystemProperties", OPERATION_MS, |op| unsafe { HcsGetComputeSystemProperties(system.0, op, ptr) })
}

/// Changes a running system: `request` is an HCS modify request (ResourcePath, RequestType,
/// Settings), e.g. the GPU assignment.
pub fn modify(id: &str, request: &str) -> Result<()> {
    let system = System::open(id)?;
    let request = wide(request);
    // SAFETY: the request string outlives the call; no identity handle.
    operation("HcsModifyComputeSystem", OPERATION_MS, |op| unsafe { HcsModifyComputeSystem(system.0, op, request.as_ptr(), std::ptr::null_mut()) }).map(|_| ())
}

/// Lets the VM's worker process open a file (a VHDX, an ISO): adds the VM's identity to the
/// file's access control list.
pub fn grant_vm_access(id: &str, file: &Path) -> Result<()> {
    let (id, file) = (wide(id), wide(&file.to_string_lossy()));
    // SAFETY: both strings are NUL-terminated and outlive the call.
    let hr = unsafe { HcsGrantVmAccess(id.as_ptr(), file.as_ptr()) };
    check("HcsGrantVmAccess", hr, String::new())
}

// ---- the Host Compute Network ------------------------------------------------------------------

/// "8c70402c-2dc8-41df-8b36-7e7938adbcb6" (braces allowed) as a GUID.
fn guid(call: &'static str, s: &str) -> Result<Guid> {
    let hex: String = s.chars().filter(char::is_ascii_hexdigit).collect();
    if hex.len() != 32 {
        return Err(Error { call, hresult: 0x8007_0057, document: format!("not a GUID: {s}") });
    }
    let n = |a: usize, b: usize| u64::from_str_radix(&hex[a..b], 16).unwrap_or(0);
    let mut tail = [0u8; 8];
    for (i, t) in tail.iter_mut().enumerate() {
        *t = n(16 + i * 2, 18 + i * 2) as u8;
    }
    Ok(Guid(n(0, 8) as u32, n(8, 12) as u16, n(12, 16) as u16, tail))
}

/// One HCN call: its result string, or its error record.
fn hcn(call: &'static str, f: impl FnOnce(*mut *mut u16, *mut *mut u16) -> Hresult) -> Result<String> {
    let (mut out, mut err) = (std::ptr::null_mut(), std::ptr::null_mut());
    let hr = f(&mut out, &mut err);
    // SAFETY: HCN allocated both strings (or left them null) and hands them to the caller.
    let (out, err) = unsafe { (take(out, free_co), take(err, free_co)) };
    check(call, hr, err)?;
    Ok(out)
}

struct Network(Handle);

impl Network {
    fn open(id: &str) -> Result<Self> {
        let g = guid("HcnOpenNetwork", id)?;
        let mut h = std::ptr::null_mut();
        // SAFETY: g and h outlive the call.
        hcn("HcnOpenNetwork", |_, err| unsafe { HcnOpenNetwork(&g, &mut h, err) })?;
        Ok(Self(h))
    }

    fn properties(&self) -> Result<String> {
        // SAFETY: the handle is open; "{}" asks for everything.
        hcn("HcnQueryNetworkProperties", |out, err| unsafe { HcnQueryNetworkProperties(self.0, wide("{}").as_ptr(), out, err) })
    }
}

impl Drop for Network {
    fn drop(&mut self) {
        // SAFETY: the handle came from HcnOpenNetwork / HcnCreateNetwork.
        unsafe { HcnCloseNetwork(self.0) };
    }
}

/// The ids of the networks HCN knows, as its JSON array.
pub fn enumerate_networks() -> Result<String> {
    // SAFETY: "{}" asks for everything.
    hcn("HcnEnumerateNetworks", |out, err| unsafe { HcnEnumerateNetworks(wide("{}").as_ptr(), out, err) })
}

/// Creates a network (a virtual switch, for "ICS" with NAT, DHCP and DNS) and returns its
/// properties.
pub fn create_network(id: &str, settings: &str) -> Result<String> {
    let (g, settings) = (guid("HcnCreateNetwork", id)?, wide(settings));
    let mut h = std::ptr::null_mut();
    // SAFETY: g, settings and h outlive the call.
    hcn("HcnCreateNetwork", |_, err| unsafe { HcnCreateNetwork(&g, settings.as_ptr(), &mut h, err) })?;
    Network(h).properties()
}

pub fn network_properties(id: &str) -> Result<String> {
    Network::open(id)?.properties()
}

pub fn delete_network(id: &str) -> Result<()> {
    let g = guid("HcnDeleteNetwork", id)?;
    // SAFETY: g outlives the call.
    hcn("HcnDeleteNetwork", |_, err| unsafe { HcnDeleteNetwork(&g, err) }).map(|_| ())
}

/// Creates an endpoint on a network (what a VM's network adapter plugs into) and returns its
/// properties; the MAC address HCN picked is in there.
pub fn create_endpoint(network_id: &str, endpoint_id: &str, settings: &str) -> Result<String> {
    let network = Network::open(network_id)?;
    let (g, settings) = (guid("HcnCreateEndpoint", endpoint_id)?, wide(settings));
    let mut h = std::ptr::null_mut();
    // SAFETY: the network handle is open; g, settings and h outlive the call.
    hcn("HcnCreateEndpoint", |_, err| unsafe { HcnCreateEndpoint(network.0, &g, settings.as_ptr(), &mut h, err) })?;
    // SAFETY: h is the endpoint just created; closed right after the query.
    let props = hcn("HcnQueryEndpointProperties", |out, err| unsafe { HcnQueryEndpointProperties(h, wide("{}").as_ptr(), out, err) });
    unsafe { HcnCloseEndpoint(h) };
    props
}

pub fn delete_endpoint(id: &str) -> Result<()> {
    let g = guid("HcnDeleteEndpoint", id)?;
    // SAFETY: g outlives the call.
    hcn("HcnDeleteEndpoint", |_, err| unsafe { HcnDeleteEndpoint(&g, err) }).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_from_text() {
        let g = guid("test", "{8c70402c-2dc8-41df-8b36-7e7938adbcb6}").unwrap();
        assert_eq!((g.0, g.1, g.2), (0x8c70_402c, 0x2dc8, 0x41df));
        assert_eq!(g.3, [0x8b, 0x36, 0x7e, 0x79, 0x38, 0xad, 0xbc, 0xb6]);
        assert!(guid("test", "not-a-guid").is_err());
    }
}
