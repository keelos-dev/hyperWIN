//! Host Device Virtualization (vmdevicehost.dll): a PCI device of a VM, emulated by this
//! process.
//!
//! Hyper-V has no sound card to give a VM; with this a process gives it any PCI device, and
//! answers the guest's accesses to it itself. The VM's document names the device under
//! `Devices.FlexibleIov` with `HostingModel: "External"`; between creating the compute system
//! and starting it, the process says it is the host of those devices ([`Host::new`]) and gives
//! each its emulator ([`Host::add`]). The devices live as long as the [`Host`], which has to be
//! as long as the VM runs.
//!
//! Seen on Windows 10 22H2 (19045): the calls are exported by vmdevicehost.dll, not by
//! computecore.dll as Microsoft's reference says. The guest sees the device on Hyper-V's
//! virtual PCI bus and can only be interrupted by messages (MSI), not by a line.

use super::{check, guid, Guid, Handle, Hresult, Result, System};
use std::ffi::c_void;

/// What a PCI device says it is.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PnpId {
    pub vendor: u16,
    pub device: u16,
    pub revision: u8,
    pub prog_if: u8,
    pub sub_class: u8,
    pub base_class: u8,
    pub sub_vendor: u16,
    pub sub_system: u16,
}

/// A PCI device's emulator. The calls come on the device host's threads, any of them at any
/// time.
pub trait PciDevice: Send + Sync + 'static {
    fn id(&self) -> PnpId;
    /// What each base address register answers after all ones were written to it: its size and
    /// kind (0xFFFF_F000 is 4 KiB of 32-bit memory), 0 for one that is not there.
    fn bars(&self) -> [u32; 6];
    /// The device exists in the VM's device host: the handle for guest memory and interrupts.
    fn attached(&self, _device: Device) {}
    /// The VM's processors are about to run / to stop.
    fn start(&self) {}
    fn stop(&self) {}
    fn read_config(&self, offset: u32) -> u32;
    fn write_config(&self, offset: u32, value: u32);
    /// The guest read from / wrote to one of the device's memory ranges.
    fn read(&self, bar: u8, offset: u64, data: &mut [u8]);
    fn write(&self, bar: u8, offset: u64, data: &[u8]);
}

const TYPE_PCI: i32 = 1;
const INTERFACE_VERSION_1: i32 = 1;
const S_OK: Hresult = 0;

#[repr(C)]
struct Interface {
    version: i32,
    initialize: unsafe extern "system" fn(*mut c_void) -> Hresult,
    teardown: unsafe extern "system" fn(*mut c_void),
    set_configuration: unsafe extern "system" fn(*mut c_void, u32, *const *const u16) -> Hresult,
    get_details: unsafe extern "system" fn(*mut c_void, *mut PnpId, u32, *mut u32) -> Hresult,
    start: unsafe extern "system" fn(*mut c_void) -> Hresult,
    stop: unsafe extern "system" fn(*mut c_void),
    read_config: unsafe extern "system" fn(*mut c_void, u32, *mut u32) -> Hresult,
    write_config: unsafe extern "system" fn(*mut c_void, u32, u32) -> Hresult,
    read_memory: unsafe extern "system" fn(*mut c_void, i32, u64, u64, *mut u8) -> Hresult,
    write_memory: unsafe extern "system" fn(*mut c_void, i32, u64, u64, *const u8) -> Hresult,
}

/// vmdevicehost.dll's calls. Looked up when first needed, not linked: a Windows without that
/// file must still run hyperWIN (its VMs then have no emulated devices).
#[allow(non_snake_case)]
struct Api {
    HdvInitializeDeviceHost: unsafe extern "system" fn(system: Handle, host: *mut Handle) -> Hresult,
    HdvTeardownDeviceHost: unsafe extern "system" fn(host: Handle) -> Hresult,
    HdvCreateDeviceInstance: unsafe extern "system" fn(host: Handle, kind: i32, class: *const Guid, instance: *const Guid, interface: *const c_void, context: *mut c_void, device: *mut Handle) -> Hresult,
    HdvReadGuestMemory: unsafe extern "system" fn(device: Handle, address: u64, count: u32, buffer: *mut u8) -> Hresult,
    HdvWriteGuestMemory: unsafe extern "system" fn(device: Handle, address: u64, count: u32, buffer: *const u8) -> Hresult,
    HdvDeliverGuestInterrupt: unsafe extern "system" fn(device: Handle, msi_address: u64, msi_data: u32) -> Hresult,
}

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(name: *const u16) -> Handle;
    fn GetProcAddress(module: Handle, name: *const u8) -> *const c_void;
}

fn api() -> Result<&'static Api> {
    static API: std::sync::OnceLock<Option<Api>> = std::sync::OnceLock::new();
    let load = || {
        // SAFETY: a system DLL by name; each function is given the type its header declares
        // (hypervdevicevirtualization.h). The module is never unloaded.
        unsafe {
            let module = LoadLibraryW(super::wide("vmdevicehost.dll").as_ptr());
            if module.is_null() {
                return None;
            }
            macro_rules! function {
                ($name:literal) => {{
                    let found = GetProcAddress(module, concat!($name, "\0").as_ptr());
                    if found.is_null() {
                        return None;
                    }
                    std::mem::transmute::<*const c_void, _>(found)
                }};
            }
            Some(Api {
                HdvInitializeDeviceHost: function!("HdvInitializeDeviceHost"),
                HdvTeardownDeviceHost: function!("HdvTeardownDeviceHost"),
                HdvCreateDeviceInstance: function!("HdvCreateDeviceInstance"),
                HdvReadGuestMemory: function!("HdvReadGuestMemory"),
                HdvWriteGuestMemory: function!("HdvWriteGuestMemory"),
                HdvDeliverGuestInterrupt: function!("HdvDeliverGuestInterrupt"),
            })
        }
    };
    // ERROR_MOD_NOT_FOUND as an HRESULT
    API.get_or_init(load).as_ref().ok_or(super::Error { call: "vmdevicehost.dll", hresult: 0x8007_007E, document: "this Windows has no device virtualization for VMs".into() })
}

#[link(name = "ole32")]
extern "system" {
    fn CoInitializeEx(reserved: *const c_void, coinit: u32) -> Hresult;
}

#[link(name = "advapi32")]
extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(text: *const u16, revision: u32, descriptor: *mut *mut c_void, size: *mut u32) -> i32;
    fn GetSecurityDescriptorSacl(descriptor: *const c_void, present: *mut i32, sacl: *mut *mut c_void, defaulted: *mut i32) -> i32;
    fn SetSecurityInfo(handle: Handle, object_type: u32, info: u32, owner: *const c_void, group: *const c_void, dacl: *const c_void, sacl: *const c_void) -> u32;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> Handle;
}

/// Lets the VM's worker process open this one.
///
/// The worker process hands this process the VM's memory and takes its events, for which it
/// opens this process. A process of LocalSystem (the service, and what the service starts) is
/// labelled with the "system" integrity level, above the worker's, and Windows refuses the
/// open whatever the access list says: HdvInitializeDeviceHost then answers "access denied"
/// (seen on Windows 10 22H2; an administrator's process, labelled "high", is let in). So this
/// process's label is lowered to "high". Who may open the process is still its access list's
/// business: the system and the administrators, as before.
unsafe fn open_to_worker() {
    const SE_KERNEL_OBJECT: u32 = 6;
    const LABEL_SECURITY_INFORMATION: u32 = 0x10;
    let mut descriptor = std::ptr::null_mut();
    // a mandatory label: no write up, no read up, high
    if ConvertStringSecurityDescriptorToSecurityDescriptorW(super::wide("S:(ML;;NWNR;;;HI)").as_ptr(), 1, &mut descriptor, std::ptr::null_mut()) == 0 {
        return;
    }
    let (mut present, mut defaulted, mut sacl) = (0, 0, std::ptr::null_mut());
    if GetSecurityDescriptorSacl(descriptor, &mut present, &mut sacl, &mut defaulted) != 0 && present != 0 {
        let failed = SetSecurityInfo(GetCurrentProcess(), SE_KERNEL_OBJECT, LABEL_SECURITY_INFORMATION, std::ptr::null(), std::ptr::null(), std::ptr::null(), sacl);
        LABEL.store(failed, std::sync::atomic::Ordering::Relaxed);
    }
    super::LocalFree(descriptor);
}

/// The error of lowering this process's integrity label, when it failed.
static LABEL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The context of every callback: a thin pointer to the device's emulator.
type Context = Box<dyn PciDevice>;

// SAFETY (all callbacks): the context is the `*const Context` given to HdvCreateDeviceInstance,
// alive until the Host is dropped, which tears the device host down first.
unsafe fn device<'a>(context: *mut c_void) -> &'a dyn PciDevice {
    &**context.cast::<Context>()
}

unsafe extern "system" fn initialize(_: *mut c_void) -> Hresult {
    S_OK
}

unsafe extern "system" fn teardown(_: *mut c_void) {}

unsafe extern "system" fn set_configuration(_: *mut c_void, _: u32, _: *const *const u16) -> Hresult {
    S_OK
}

unsafe extern "system" fn get_details(context: *mut c_void, id: *mut PnpId, count: u32, bars: *mut u32) -> Hresult {
    let d = device(context);
    *id = d.id();
    for (i, bar) in d.bars().into_iter().enumerate().take(count as usize) {
        *bars.add(i) = bar;
    }
    S_OK
}

unsafe extern "system" fn start(context: *mut c_void) -> Hresult {
    device(context).start();
    S_OK
}

unsafe extern "system" fn stop(context: *mut c_void) {
    device(context).stop();
}

unsafe extern "system" fn read_config(context: *mut c_void, offset: u32, value: *mut u32) -> Hresult {
    *value = device(context).read_config(offset);
    S_OK
}

unsafe extern "system" fn write_config(context: *mut c_void, offset: u32, value: u32) -> Hresult {
    device(context).write_config(offset, value);
    S_OK
}

unsafe extern "system" fn read_memory(context: *mut c_void, bar: i32, offset: u64, length: u64, value: *mut u8) -> Hresult {
    device(context).read(bar as u8, offset, std::slice::from_raw_parts_mut(value, length as usize));
    S_OK
}

unsafe extern "system" fn write_memory(context: *mut c_void, bar: i32, offset: u64, length: u64, value: *const u8) -> Hresult {
    device(context).write(bar as u8, offset, std::slice::from_raw_parts(value, length as usize));
    S_OK
}

static INTERFACE: Interface = Interface {
    version: INTERFACE_VERSION_1,
    initialize,
    teardown,
    set_configuration,
    get_details,
    start,
    stop,
    read_config,
    write_config,
    read_memory,
    write_memory,
};

/// A device in the device host: what its emulator reaches the guest with.
#[derive(Clone, Copy)]
pub struct Device(Handle);

// SAFETY: the handle is an opaque token the HDV calls take from any thread.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Device {
    /// Reads the guest's memory at a guest physical address.
    pub fn read_guest(&self, address: u64, buffer: &mut [u8]) -> Result<()> {
        // SAFETY: the buffer is ours for its length.
        let hr = unsafe { (api()?.HdvReadGuestMemory)(self.0, address, buffer.len() as u32, buffer.as_mut_ptr()) };
        check("HdvReadGuestMemory", hr, String::new())
    }

    pub fn write_guest(&self, address: u64, buffer: &[u8]) -> Result<()> {
        // SAFETY: as above.
        let hr = unsafe { (api()?.HdvWriteGuestMemory)(self.0, address, buffer.len() as u32, buffer.as_ptr()) };
        check("HdvWriteGuestMemory", hr, String::new())
    }

    /// Raises a message signaled interrupt: the address and data the guest programmed into the
    /// device (its MSI or MSI-X registers).
    pub fn interrupt(&self, msi_address: u64, msi_data: u32) -> Result<()> {
        // SAFETY: plain values.
        let hr = unsafe { (api()?.HdvDeliverGuestInterrupt)(self.0, msi_address, msi_data) };
        check("HdvDeliverGuestInterrupt", hr, String::new())
    }
}

/// This process as the host of a VM's external devices. Dropping it takes the devices away.
pub struct Host {
    handle: Handle,
    _system: System,
    devices: Vec<*mut Context>,
}

// SAFETY: the handles are tokens; the contexts are only touched by the callbacks and by drop.
unsafe impl Send for Host {}
unsafe impl Sync for Host {}

impl Host {
    /// For a compute system that is created and not started yet.
    pub fn new(vm_id: &str) -> Result<Self> {
        let api = api()?;
        let system = System::open(vm_id)?;
        let mut handle = std::ptr::null_mut();
        // SAFETY: the device host talks COM to the VM's worker process; a thread that has COM
        // already keeps its own setting. The system handle is open and stays so in the Host.
        let hr = unsafe {
            const COINIT_MULTITHREADED: u32 = 0;
            CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED);
            open_to_worker();
            (api.HdvInitializeDeviceHost)(system.0, &mut handle)
        };
        // (said with the failure: it may be why)
        let label = LABEL.load(std::sync::atomic::Ordering::Relaxed);
        check("HdvInitializeDeviceHost", hr, if label == 0 { String::new() } else { format!("(this process's integrity label could not be lowered: error {label})") })?;
        Ok(Self { handle, _system: system, devices: Vec::new() })
    }

    /// Adds a device: `instance` is its key under `Devices.FlexibleIov` in the VM's document,
    /// `class` that entry's `EmulatorId` (both GUIDs).
    pub fn add(&mut self, class: &str, instance: &str, device: Box<dyn PciDevice>) -> Result<Device> {
        let (class, instance) = (guid("HdvCreateDeviceInstance", class)?, guid("HdvCreateDeviceInstance", instance)?);
        let context: *mut Context = Box::into_raw(Box::new(device));
        self.devices.push(context);
        let mut handle = std::ptr::null_mut();
        // SAFETY: the GUIDs outlive the call; the interface table is static; the context lives
        // until this Host is dropped.
        let hr = unsafe { (api()?.HdvCreateDeviceInstance)(self.handle, TYPE_PCI, &class, &instance, std::ptr::from_ref(&INTERFACE).cast(), context.cast(), &mut handle) };
        check("HdvCreateDeviceInstance", hr, String::new())?;
        let device = Device(handle);
        // SAFETY: just made, not freed before drop.
        unsafe { (*context).attached(device) };
        Ok(device)
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        // SAFETY: the host handle came from HdvInitializeDeviceHost; after the teardown no
        // callback runs, so the contexts can go.
        unsafe {
            if let Ok(api) = api() {
                (api.HdvTeardownDeviceHost)(self.handle);
            }
            for context in self.devices.drain(..) {
                drop(Box::from_raw(context));
            }
        }
    }
}
