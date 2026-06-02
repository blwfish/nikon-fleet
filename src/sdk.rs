//! Nikon Remote SDK v2 — FFI and safe Rust wrapper.
//!
//! This is the only `unsafe` module in nikon-fleet. The intent is to keep the
//! surface area of `unsafe` as small and well-named as possible; everything
//! outside this file is plain safe Rust.
//!
//! ## Lifecycle
//!
//! ```text
//!   Sdk::open(bundle_path)         // dlopen the bundle, resolve symbols
//!     .initialize()                // USB reset → InitializeSDK → IOKit notifications armed
//!   sdk.devices()                  // EnumDevices (with CF run-loop pump) → Vec<DeviceInfo>
//!   sdk.connect(device_id)         // ConnectDevice → capability list
//!     .read_capability(cap_id)     // GetCapability(Value) → JSON
//!   (drop Device → DisconnectDevice)
//!   (drop Sdk → library unloaded; FreeSDK deliberately NOT called — see Drop impl)
//! ```
//!
//! ## Why the USB reset + run-loop pump?
//!
//! The SDK registers `IOServiceAddMatchingNotification` during `InitializeSDK`.
//! That notification only fires for devices that appear *after* registration.
//! Cameras already connected at process startup are invisible. We fix this by:
//!  1. Resetting the camera's USB connection (via rusb) immediately before
//!     `InitializeSDK`, so the reconnect event fires after registration.
//!  2. Pumping the CoreFoundation run loop between `EnumDevices` retries,
//!     so the IOKit callback that adds the device to the SDK's internal list
//!     is actually delivered.
//!
//! ## Threading
//!
//! The SDK is not documented to be thread-safe. We are single-threaded
//! everywhere; if concurrency is ever needed, gate calls behind a Mutex here.
//!
//! ## SDK Path
//!
//! The bundle path is passed in at construction so this module doesn't
//! hardcode any user path. See `scripts/setup-sdk-runtime.sh` for the
//! recommended layout (sdk-runtime/ at the project root).

use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};
use std::ptr;

use libloading::{Library, Symbol};
use serde_json::{Value, json};
use thiserror::Error;

const NIKON_VID: u16 = 0x04B0;

// ─────────────────────────────────────────────────────────────────────────
// CoreFoundation run loop — delivers pending IOKit notifications.
// ─────────────────────────────────────────────────────────────────────────

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_source_handled: u8) -> i32;
    static kCFRunLoopDefaultMode: *const c_void;
}

fn pump_cf_runloop(duration: std::time::Duration) {
    // returnAfterSourceHandled=0: process all pending events, not just the first.
    unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, duration.as_secs_f64(), 0); }
}

// ─────────────────────────────────────────────────────────────────────────
// C types — must mirror Maid3.h exactly.
// ─────────────────────────────────────────────────────────────────────────

#[repr(C)]
struct NkMAIDDeviceInfo {
    id: u32,
    name: [c_char; 64],
    availability: bool,
    // 3 bytes of implicit padding under repr(C).
    connected_pid: u32,
    version: [c_char; 64],
}

#[repr(C)]
struct NkMAIDEnumDevices {
    ul_elements: u32,
    ul_value: u32,
    p_device_data: *mut NkMAIDDeviceInfo,
}

#[repr(C)]
struct NkMAIDCapInfo {
    ul_id: u32,
    ul_type: u32,
    ul_visibility: u32,
    ul_operations: u32,
    sz_description: [c_char; 256],
}

#[repr(C)]
struct NkMAIDEnumCapInfo {
    p_cap_array: *mut NkMAIDCapInfo,
    ul_cap_count: u32,
    ul_allocation_size: u32,
}

/// InitializeSDK requires non-null callbacks even for a snapshot-only workflow.
#[repr(C)]
struct NkMAIDCSCallback {
    p_ui_req_proc: *mut c_void,
    p_event_proc: *mut c_void,
    p_progress_proc: *mut c_void,
    p_data_proc: *mut c_void,
    p_live_view_data_proc: *mut c_void,
    ref_proc: *mut c_void,
}

// eNkMAIDDataType values.
const DT_NULL: u32 = 0;
#[allow(dead_code)] const DT_BOOLEAN: u32 = 1;
const DT_INTEGER: u32 = 2;
const DT_UNSIGNED: u32 = 3;
const DT_BOOLEAN_PTR: u32 = 4;
const DT_INTEGER_PTR: u32 = 5;
const DT_UNSIGNED_PTR: u32 = 6;
const DT_FLOAT_PTR: u32 = 7;
const DT_STRING_PTR: u32 = 11;
const DT_RANGE_PTR: u32 = 13;
const DT_ENUM_PTR: u32 = 15;

const GET_VALUE: u32 = 0; // eNkSDKGetSettingRequestType

// ─────────────────────────────────────────────────────────────────────────
// SDK function pointer types.
// ─────────────────────────────────────────────────────────────────────────

type MAIDAllocateMemory = unsafe extern "C" fn(size: libc::size_t) -> *mut c_void;
type MAIDFreeMemory = unsafe extern "C" fn(ptr: *mut c_void);

type FnInitializeSDK = unsafe extern "C" fn(
    alloc_fn: MAIDAllocateMemory,
    free_fn: MAIDFreeMemory,
    callback: *const NkMAIDCSCallback,
    pp_device_list: *mut *mut NkMAIDEnumDevices,
    pp_cap_info: *mut *mut NkMAIDEnumCapInfo,
) -> i32;
type FnFreeSDK = unsafe extern "C" fn() -> i32;
type FnEnumDevices = unsafe extern "C" fn(
    pp_devices: *mut *mut NkMAIDEnumDevices,
    p_proc: *mut c_void,
    nk_ref: *mut c_void,
) -> i32;
type FnConnectDevice =
    unsafe extern "C" fn(ul_device_id: u32, pp_cap_info: *mut *mut NkMAIDEnumCapInfo) -> i32;
type FnDisconnectDevice = unsafe extern "C" fn() -> i32;
type FnGetCapability = unsafe extern "C" fn(
    ul_capability_id: u32,
    e_type: u32,
    pp_data: *mut *mut c_void,
    p_data_type: *mut u32,
) -> i32;

// ─────────────────────────────────────────────────────────────────────────
// SDK allocator and no-op callbacks.
// ─────────────────────────────────────────────────────────────────────────

unsafe extern "C" fn sdk_alloc(size: libc::size_t) -> *mut c_void {
    unsafe { libc::malloc(size) }
}

unsafe extern "C" fn sdk_free(ptr: *mut c_void) {
    unsafe { libc::free(ptr) }
}

unsafe extern "C" fn cb_noop() {}

// ─────────────────────────────────────────────────────────────────────────
// Errors
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum SdkError {
    #[error("failed to load SDK bundle at {path:?}: {source}")]
    Load { path: PathBuf, source: libloading::Error },
    #[error("missing SDK symbol `{0}`: {1}")]
    Symbol(String, libloading::Error),
    #[error("SDK call `{call}` returned error code {code}")]
    SdkCall { call: &'static str, code: i32 },
    #[error("UTF-8 conversion failed: {0}")]
    Utf8(#[from] std::str::Utf8Error),
}

fn check(call: &'static str, code: i32) -> Result<(), SdkError> {
    if code < 0 { Err(SdkError::SdkCall { call, code }) } else { Ok(()) }
}

// ─────────────────────────────────────────────────────────────────────────
// USB descriptor helpers (rusb / IOKit layer)
//
// The Nikon SDK returns empty strings for both firmware version and serial
// number on all tested models. We read them directly from the USB device
// descriptors instead.
// ─────────────────────────────────────────────────────────────────────────

/// Camera identity read from USB device descriptors.
#[derive(Debug, Clone)]
pub struct UsbCameraInfo {
    pub product_id: u16,
    /// `iSerialNumber` string descriptor, e.g. `"0000003023668"`.
    pub serial: String,
    /// `bcdDevice` BCD-decoded firmware version, e.g. `"5.31"`.
    pub firmware: String,
    /// USB product string minus `"NIKON DSC "` prefix, e.g. `"Z 9"`.
    /// Matches MaidLayerConfig keys directly.
    pub model: String,
}

/// Decode a USB `bcdDevice` value (u16) to a dotted version string.
///
/// BCD encoding: each nibble is a decimal digit.
/// `0x0531` → `"5.31"`, `0x0143` → `"1.43"`, `0x0200` → `"2.00"`.
pub fn bcd_decode_version(bcd: u16) -> String {
    let major = ((bcd >> 12) & 0xF) * 10 + ((bcd >> 8) & 0xF);
    let minor = ((bcd >> 4) & 0xF) * 10 + (bcd & 0xF);
    format!("{major}.{minor:02}")
}

fn model_from_product_string(product: &str) -> String {
    product.strip_prefix("NIKON DSC ").unwrap_or(product).to_owned()
}

fn nikon_usb_devices() -> Vec<rusb::Device<rusb::GlobalContext>> {
    let list = match rusb::DeviceList::new() {
        Ok(l) => l,
        Err(_) => return Vec::new(),
    };
    list.iter()
        .filter(|d| d.device_descriptor().map(|desc| desc.vendor_id() == NIKON_VID).unwrap_or(false))
        .collect()
}

fn read_usb_string(handle: &rusb::DeviceHandle<rusb::GlobalContext>, idx: u8) -> String {
    if idx == 0 { return String::new(); }
    let timeout = std::time::Duration::from_secs(1);
    let lang = match handle.read_languages(timeout).ok().and_then(|l| l.into_iter().next()) {
        Some(l) => l,
        None => return String::new(),
    };
    handle.read_string_descriptor(lang, idx, timeout).unwrap_or_default()
}

/// List all Nikon cameras visible on USB with firmware version and serial number.
pub fn usb_camera_list() -> Vec<UsbCameraInfo> {
    let mut out = Vec::new();
    for device in nikon_usb_devices() {
        let desc = match device.device_descriptor() { Ok(d) => d, Err(_) => continue };
        let handle = match device.open() { Ok(h) => h, Err(_) => continue };
        let serial = read_usb_string(&handle, desc.serial_number_string_index().unwrap_or(0));
        let product = read_usb_string(&handle, desc.product_string_index().unwrap_or(0));
        let v = desc.device_version();
        let firmware = format!("{}.{:02}", v.0, u32::from(v.1) * 10 + u32::from(v.2));
        out.push(UsbCameraInfo {
            product_id: desc.product_id(),
            serial,
            firmware,
            model: model_from_product_string(&product),
        });
    }
    out
}

/// Reset all connected Nikon USB cameras via a USB port reset.
///
/// This forces a disconnect/reconnect cycle, making the camera appear as a
/// "new" device to IOKit's matching notification system. Call immediately
/// before `InitializeSDK` so the SDK's notification is registered before the
/// camera completes its reconnection.
fn reset_nikon_usb_cameras() -> usize {
    let mut count = 0;
    for device in nikon_usb_devices() {
        if let Ok(handle) = device.open() {
            if handle.reset().is_ok() {
                count += 1;
            }
        }
    }
    count
}

// ─────────────────────────────────────────────────────────────────────────
// Sdk — the loaded library + initialized session.
// ─────────────────────────────────────────────────────────────────────────

/// Resolved SDK entry points. Created with `Sdk::open`, kept alive for the
/// duration of any camera I/O.
pub struct Sdk {
    // Field order matters for Drop: function pointers (borrowing from `_lib`)
    // must drop before the Library. Rust drops fields in declaration order.
    initialize: FnInitializeSDK,
    free_sdk: FnFreeSDK,
    enum_devices: FnEnumDevices,
    connect_device: FnConnectDevice,
    disconnect_device: FnDisconnectDevice,
    get_capability: FnGetCapability,
    initialized: bool,
    _lib: Library, // last — outlives the function pointers above
}

/// Single device entry returned by `Sdk::devices()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: u32,
    pub name: String,
    pub available: bool,
    pub connected_pid: u32,
    pub version: String,
}

/// Live capability description (id + ops flags + description).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityInfo {
    pub id: u32,
    pub kind: u32,
    pub visibility: u32,
    pub operations: u32,
    pub description: String,
}

impl Sdk {
    /// Load the SDK bundle's executable at `bundle_exe_path`. Does NOT
    /// initialize a session — call `initialize()` next.
    ///
    /// Typical path:
    ///   `<sdk-runtime>/TypeCommon Module.bundle/Contents/MacOS/TypeCommon Module`
    pub fn open<P: AsRef<Path>>(bundle_exe_path: P) -> Result<Self, SdkError> {
        let path = bundle_exe_path.as_ref().to_path_buf();
        let lib = unsafe { Library::new(&path) }
            .map_err(|e| SdkError::Load { path: path.clone(), source: e })?;

        unsafe fn resolve<T: Copy>(lib: &Library, name: &[u8]) -> Result<T, SdkError> {
            let sym: Symbol<T> = unsafe { lib.get(name) }
                .map_err(|e| SdkError::Symbol(String::from_utf8_lossy(name).into_owned(), e))?;
            Ok(*sym)
        }

        unsafe {
            Ok(Sdk {
                initialize: resolve(&lib, b"InitializeSDK\0")?,
                free_sdk: resolve(&lib, b"FreeSDK\0")?,
                enum_devices: resolve(&lib, b"EnumDevices\0")?,
                connect_device: resolve(&lib, b"ConnectDevice\0")?,
                disconnect_device: resolve(&lib, b"DisconnectDevice\0")?,
                get_capability: resolve(&lib, b"GetCapability\0")?,
                initialized: false,
                _lib: lib,
            })
        }
    }

    /// Initialize an SDK session. Must be called before any other method.
    ///
    /// Resets all connected Nikon cameras via USB before calling
    /// `InitializeSDK`, so the SDK's IOKit matching notifications fire when
    /// the cameras reconnect (see module-level doc for the full explanation).
    pub fn initialize(&mut self) -> Result<(), SdkError> {
        // Reset first, then immediately call InitializeSDK. The camera will
        // finish reconnecting while InitializeSDK is running or shortly after,
        // and devices() will pick it up via the run-loop-pumped retry loop.
        reset_nikon_usb_cameras();

        let stub: *mut c_void = cb_noop as *mut c_void;
        let callback = NkMAIDCSCallback {
            p_ui_req_proc: stub,
            p_event_proc: stub,
            p_progress_proc: stub,
            p_data_proc: stub,
            p_live_view_data_proc: stub,
            ref_proc: ptr::null_mut(),
        };
        let mut device_list: *mut NkMAIDEnumDevices = ptr::null_mut();
        let code = unsafe {
            (self.initialize)(sdk_alloc, sdk_free, &callback, &mut device_list, ptr::null_mut())
        };
        check("InitializeSDK", code)?;
        self.initialized = true;
        if !device_list.is_null() {
            unsafe {
                let dl = &*device_list;
                if !dl.p_device_data.is_null() { sdk_free(dl.p_device_data as *mut c_void); }
                sdk_free(device_list as *mut c_void);
            }
        }
        Ok(())
    }

    /// Enumerate USB-attached Nikon cameras.
    ///
    /// Retries with CoreFoundation run-loop pumps between attempts so that
    /// the IOKit matching notification (fired when the camera completes its
    /// post-reset USB reconnection) is delivered to the SDK's callback before
    /// we give up.
    pub fn devices(&self) -> Result<Vec<DeviceInfo>, SdkError> {
        let mut list: *mut NkMAIDEnumDevices = ptr::null_mut();
        let mut code = 0i32;
        for _ in 0..10u32 {
            list = ptr::null_mut();
            code = unsafe {
                (self.enum_devices)(&mut list, ptr::null_mut(), ptr::null_mut())
            };
            let elements = if list.is_null() { 0 } else { unsafe { (*list).ul_elements } };
            if elements > 0 { break; }
            if !list.is_null() {
                unsafe {
                    let dl = &*list;
                    if !dl.p_device_data.is_null() { sdk_free(dl.p_device_data as *mut c_void); }
                    sdk_free(list as *mut c_void);
                }
                list = ptr::null_mut();
            }
            pump_cf_runloop(std::time::Duration::from_millis(300));
        }
        check("EnumDevices", code)?;
        let mut out = Vec::new();
        if !list.is_null() {
            unsafe {
                let dl = &*list;
                for i in 0..dl.ul_elements as usize {
                    let info = &*dl.p_device_data.add(i);
                    out.push(DeviceInfo {
                        id: info.id,
                        name: cstr_to_string(&info.name),
                        available: info.availability,
                        connected_pid: info.connected_pid,
                        version: cstr_to_string(&info.version),
                    });
                }
                if !dl.p_device_data.is_null() { sdk_free(dl.p_device_data as *mut c_void); }
                sdk_free(list as *mut c_void);
            }
        }
        Ok(out)
    }

    /// Open a session on the given device. Returns a `Device` that holds
    /// the connection and disconnects on drop.
    pub fn connect(&mut self, device_id: u32) -> Result<Device<'_>, SdkError> {
        let mut cap_info: *mut NkMAIDEnumCapInfo = ptr::null_mut();
        let code = unsafe { (self.connect_device)(device_id, &mut cap_info) };
        check("ConnectDevice", code)?;
        let capabilities = unsafe { take_capabilities(cap_info) };
        Ok(Device { sdk: self, capabilities })
    }
}

impl Drop for Sdk {
    fn drop(&mut self) {
        // FreeSDK is deliberately NOT called. Calling it leaves the camera in
        // a state where subsequent process invocations cannot enumerate it via
        // InitializeSDK + EnumDevices until the USB cable is replugged. Letting
        // the OS reap the process and its USB handles is cleaner for the CLI
        // use case. The USB reset at the start of the next initialize() call
        // sidesteps this entirely.
        let _ = self.initialized;
    }
}

/// An active camera session. Disconnects on drop.
pub struct Device<'sdk> {
    sdk: &'sdk Sdk,
    pub capabilities: Vec<CapabilityInfo>,
}

impl<'sdk> Device<'sdk> {
    /// Read the current value of one capability as a JSON value.
    pub fn read_capability(&self, capability_id: u32) -> Result<Value, SdkError> {
        let mut data_ptr: *mut c_void = ptr::null_mut();
        let mut data_type: u32 = 0;
        let code = unsafe {
            (self.sdk.get_capability)(capability_id, GET_VALUE, &mut data_ptr, &mut data_type)
        };
        check("GetCapability", code)?;
        let value = unsafe { decode_value(data_ptr, data_type) };
        if data_type >= DT_BOOLEAN_PTR && !data_ptr.is_null() {
            unsafe { sdk_free(data_ptr) };
        }
        Ok(value)
    }
}

impl<'sdk> Drop for Device<'sdk> {
    fn drop(&mut self) {
        let _ = unsafe { (self.sdk.disconnect_device)() };
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────

fn cstr_to_string(buf: &[c_char]) -> String {
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len()) };
    let nul = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..nul]).into_owned()
}

unsafe fn take_capabilities(p: *mut NkMAIDEnumCapInfo) -> Vec<CapabilityInfo> {
    if p.is_null() { return Vec::new(); }
    let info = unsafe { &*p };
    let mut out = Vec::with_capacity(info.ul_cap_count as usize);
    for i in 0..info.ul_cap_count as usize {
        let c = unsafe { &*info.p_cap_array.add(i) };
        out.push(CapabilityInfo {
            id: c.ul_id,
            kind: c.ul_type,
            visibility: c.ul_visibility,
            operations: c.ul_operations,
            description: cstr_to_string(&c.sz_description),
        });
    }
    unsafe {
        if !info.p_cap_array.is_null() { sdk_free(info.p_cap_array as *mut c_void); }
        sdk_free(p as *mut c_void);
    }
    out
}

unsafe fn decode_value(data_ptr: *mut c_void, data_type: u32) -> Value {
    match data_type {
        DT_NULL => Value::Null,
        DT_INTEGER => json!(data_ptr as i64 as i32),
        DT_UNSIGNED => json!(data_ptr as usize as u32),
        DT_BOOLEAN_PTR => {
            if data_ptr.is_null() { Value::Null } else { json!(unsafe { *(data_ptr as *const u8) } != 0) }
        }
        DT_INTEGER_PTR => {
            if data_ptr.is_null() { Value::Null } else { json!(unsafe { *(data_ptr as *const i32) }) }
        }
        DT_UNSIGNED_PTR => {
            if data_ptr.is_null() { Value::Null } else { json!(unsafe { *(data_ptr as *const u32) }) }
        }
        DT_FLOAT_PTR => {
            if data_ptr.is_null() { Value::Null } else { json!(unsafe { *(data_ptr as *const f64) }) }
        }
        DT_STRING_PTR => {
            if data_ptr.is_null() { Value::Null } else {
                let cs = unsafe { CStr::from_ptr(data_ptr as *const c_char) };
                json!(cs.to_string_lossy().into_owned())
            }
        }
        DT_RANGE_PTR | DT_ENUM_PTR => json!({ "_unsupported_type": data_type }),
        _ => json!({ "_unknown_type": data_type }),
    }
}
