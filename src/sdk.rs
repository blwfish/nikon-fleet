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

use std::collections::HashMap;
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

/// NkMAIDEnum — returned by GetCapability for kNkMAIDCapType_Enum capabilities.
///
/// Layout verified against Maid3.h (Mac 64-bit / LP64):
///   offsets 0–15: four u32 fields; offset 16: i16; 6-byte pad; offset 24: *mut c_void.
#[repr(C)]
struct NkMAIDEnum {
    ul_type: u32,          // eNkMAIDArrayType of the element data
    ul_elements: u32,      // number of valid choices
    ul_value: u32,         // CURRENT index — the only field changed on Set
    ul_default: u32,       // default index
    w_physical_bytes: i16, // bytes per element in pData
    // 6 bytes implicit padding so *mut c_void lands at offset 24
    p_data: *mut c_void,   // SDK-allocated element array; NULL when writing
}

/// NkMAIDRange — returned by GetCapability for kNkMAIDCapType_Range capabilities.
#[repr(C)]
struct NkMAIDRange {
    lf_value: f64,
    lf_default: f64,
    ul_value_index: u32,
    ul_default_index: u32,
    lf_lower: f64,
    lf_upper: f64,
    ul_steps: u32, // 0 = continuous; ≥2 = discrete steps
    // 4 bytes implicit padding (struct alignment = 8 due to f64 fields)
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

// eNkMAIDDataType values (from Maid3.h eNkMAIDDataType enum).
const DT_NULL: u32 = 0;
#[allow(dead_code)] const DT_BOOLEAN: u32 = 1;
const DT_INTEGER: u32 = 2;
const DT_UNSIGNED: u32 = 3;
const DT_BOOLEAN_PTR: u32 = 4;
const DT_INTEGER_PTR: u32 = 5;
const DT_UNSIGNED_PTR: u32 = 6;
const DT_FLOAT_PTR: u32 = 7;
// 8=PointPtr 9=SizePtr 10=RectPtr — not used by settings capabilities
const DT_STRING_PTR: u32 = 11;
const DT_DATETIME_PTR: u32 = 12;
// 13=CallbackPtr — not returned by GetCapability
const DT_RANGE_PTR: u32 = 14;
const DT_ARRAY_PTR: u32 = 15; // needs GetArray, not GetCapability
const DT_ENUM_PTR: u32 = 16;

const GET_VALUE: u32 = 0; // eNkSDKGetSettingRequestType

// kNkMAIDCapOperation bits — ulOperations field in NkMAIDCapInfo.
pub const OP_GET: u32 = 0x0002;
pub const OP_SET: u32 = 0x0004;

// kNkMAIDCapType_* — capability type codes (CapabilityInfo.kind).
// Used by write_capability to choose the right SetCapability data type.
const CAP_TYPE_BOOLEAN: u32 = 1;
const CAP_TYPE_INTEGER: u32 = 2;
const CAP_TYPE_UNSIGNED: u32 = 3;
const CAP_TYPE_FLOAT: u32 = 4;
const CAP_TYPE_STRING: u32 = 8;
const CAP_TYPE_ENUM: u32 = 12;
const CAP_TYPE_RANGE: u32 = 13;

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
type FnSetCapability = unsafe extern "C" fn(
    ul_capability_id: u32,
    p_data: *const c_void,
    ul_data_type: u32,
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
    #[error("capability kind {0} is not supported for write")]
    UnsupportedWrite(u32),
    #[error("JSON value missing required field for SetCapability")]
    InvalidValue,
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
        // rusb splits bcdDevice into (high_byte, upper_nibble, lower_nibble).
        // Reconstruct the raw BCD u16 so bcd_decode_version handles both
        // single- and two-digit majors correctly.
        let raw_bcd: u16 = (v.0 as u16) << 8 | (v.1 as u16) << 4 | (v.2 as u16);
        let firmware = bcd_decode_version(raw_bcd);
        out.push(UsbCameraInfo {
            product_id: desc.product_id(),
            serial,
            firmware,
            model: model_from_product_string(&product),
        });
    }
    out
}

/// Pair SDK devices to USB camera info by model + position within same-model group.
///
/// Both the SDK and the OS USB stack enumerate in the same physical order, so
/// the nth "Z 9" in `devices` corresponds to the nth "Z 9" in `usb`. Returns
/// `None` for the USB slot when there are more SDK devices of a model than USB
/// entries (shouldn't happen in practice, but pins the fallback).
pub fn pair_devices(
    devices: Vec<DeviceInfo>,
    usb: &[UsbCameraInfo],
) -> Vec<(DeviceInfo, Option<UsbCameraInfo>)> {
    let mut model_idx: HashMap<String, usize> = HashMap::new();
    devices.into_iter().map(|dev| {
        let idx = {
            let c = model_idx.entry(dev.name.clone()).or_insert(0);
            let i = *c;
            *c += 1;
            i
        };
        let usb_info = usb.iter().filter(|u| u.model == dev.name).nth(idx).cloned();
        (dev, usb_info)
    }).collect()
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
    set_capability: FnSetCapability,
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
                set_capability: resolve(&lib, b"SetCapability\0")?,
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
        unsafe { free_cap_value(data_ptr, data_type) };
        Ok(value)
    }

    /// Write a capability value back to the camera.
    ///
    /// `cap_kind` is the `CapabilityInfo.kind` field (kNkMAIDCapType_*), which
    /// determines which SetCapability data type and struct layout to use.
    /// `value` must be the JSON that `read_capability` previously returned for
    /// this capability (or an equivalent with the same structure).
    pub fn write_capability(&self, capability_id: u32, cap_kind: u32, value: &Value) -> Result<(), SdkError> {
        unsafe { write_cap_value(self.sdk, capability_id, cap_kind, value) }
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

/// Free a pointer returned by GetCapability, respecting the ownership rules
/// documented in the SDK sample's `freeByDataType`. EnumPtr has a nested
/// `pData` allocation that must be freed first.
unsafe fn free_cap_value(data_ptr: *mut c_void, data_type: u32) {
    if data_ptr.is_null() { return; }
    match data_type {
        DT_ENUM_PTR => {
            let st = unsafe { &*(data_ptr as *const NkMAIDEnum) };
            if !st.p_data.is_null() { unsafe { sdk_free(st.p_data) }; }
            unsafe { sdk_free(data_ptr) };
        }
        t if t >= DT_BOOLEAN_PTR => {
            unsafe { sdk_free(data_ptr) };
        }
        _ => {} // DT_NULL / by-value types — no allocation to free
    }
}

unsafe fn decode_value(data_ptr: *mut c_void, data_type: u32) -> Value {
    match data_type {
        DT_NULL => Value::Null,
        DT_INTEGER => json!(data_ptr as i64 as i32),
        DT_UNSIGNED => json!(data_ptr as usize as u32),
        DT_BOOLEAN_PTR => {
            if data_ptr.is_null() { Value::Null }
            else { json!(unsafe { *(data_ptr as *const u8) } != 0) }
        }
        DT_INTEGER_PTR => {
            if data_ptr.is_null() { Value::Null }
            else { json!(unsafe { *(data_ptr as *const i32) }) }
        }
        DT_UNSIGNED_PTR => {
            if data_ptr.is_null() { Value::Null }
            else { json!(unsafe { *(data_ptr as *const u32) }) }
        }
        DT_FLOAT_PTR => {
            if data_ptr.is_null() { Value::Null }
            else { json!(unsafe { *(data_ptr as *const f64) }) }
        }
        DT_STRING_PTR => {
            if data_ptr.is_null() { Value::Null } else {
                let cs = unsafe { CStr::from_ptr(data_ptr as *const c_char) };
                json!(cs.to_string_lossy().into_owned())
            }
        }
        DT_DATETIME_PTR => json!({ "_type": "datetime" }), // rare; not round-trip-able
        DT_ARRAY_PTR => json!({ "_type": "array" }), // requires GetArray, not GetCapability
        DT_ENUM_PTR => {
            if data_ptr.is_null() { return Value::Null; }
            let st = unsafe { &*(data_ptr as *const NkMAIDEnum) };
            let values = decode_enum_values(st);
            json!({
                "value_index": st.ul_value,
                "default_index": st.ul_default,
                "elem_count": st.ul_elements,
                "elem_type": st.ul_type,
                "elem_bytes": st.w_physical_bytes,
                "values": values,
            })
        }
        DT_RANGE_PTR => {
            if data_ptr.is_null() { return Value::Null; }
            let st = unsafe { &*(data_ptr as *const NkMAIDRange) };
            json!({
                "value": st.lf_value,
                "default": st.lf_default,
                "value_index": st.ul_value_index,
                "default_index": st.ul_default_index,
                "lower": st.lf_lower,
                "upper": st.lf_upper,
                "steps": st.ul_steps,
            })
        }
        _ => json!({ "_unknown_type": data_type }),
    }
}

/// Decode the element array of an NkMAIDEnum into a JSON array.
///
/// kNkMAIDArrayType values: 0=Boolean, 1=Integer, 2=Unsigned, 3=Float,
/// 7=PackedString (null-terminated strings packed end-to-end), 8=String.
/// Only Unsigned and Integer are common for camera settings.
fn decode_enum_values(st: &NkMAIDEnum) -> Value {
    let n = st.ul_elements as usize;
    let bytes = st.w_physical_bytes as usize;
    if st.p_data.is_null() || n == 0 { return Value::Array(vec![]); }
    match (st.ul_type, bytes) {
        (2, 4) => { // Unsigned, 4 bytes
            let s = unsafe { std::slice::from_raw_parts(st.p_data as *const u32, n) };
            Value::Array(s.iter().map(|&v| json!(v)).collect())
        }
        (2, 2) => { // Unsigned, 2 bytes
            let s = unsafe { std::slice::from_raw_parts(st.p_data as *const u16, n) };
            Value::Array(s.iter().map(|&v| json!(v as u32)).collect())
        }
        (2, 1) => { // Unsigned, 1 byte
            let s = unsafe { std::slice::from_raw_parts(st.p_data as *const u8, n) };
            Value::Array(s.iter().map(|&v| json!(v as u32)).collect())
        }
        (1, 4) => { // Integer, 4 bytes
            let s = unsafe { std::slice::from_raw_parts(st.p_data as *const i32, n) };
            Value::Array(s.iter().map(|&v| json!(v)).collect())
        }
        (7, bytes) if bytes > 0 => {
            // For PackedString, w_physical_bytes is the per-element stride:
            // each string occupies exactly `bytes` bytes in the SDK allocation.
            let total = n * bytes;
            let raw = unsafe { std::slice::from_raw_parts(st.p_data as *const u8, total) };
            let mut out = Vec::with_capacity(n);
            for chunk in raw.chunks(bytes).take(n) {
                let end = chunk.iter().position(|&b| b == 0).unwrap_or(chunk.len());
                out.push(json!(String::from_utf8_lossy(&chunk[..end]).into_owned()));
            }
            Value::Array(out)
        }
        _ => Value::Null, // unsupported element type — index still stored
    }
}

/// Build an NkMAIDEnum from the JSON that `decode_value` produced for DT_ENUM_PTR.
/// Returns `Err(InvalidValue)` if `value_index` is absent or not numeric.
fn enum_write_data(value: &Value) -> Result<NkMAIDEnum, SdkError> {
    Ok(NkMAIDEnum {
        ul_type: value["elem_type"].as_u64().unwrap_or(2) as u32,
        ul_elements: value["elem_count"].as_u64().unwrap_or(0) as u32,
        ul_value: value["value_index"].as_u64().ok_or(SdkError::InvalidValue)? as u32,
        ul_default: value["default_index"].as_u64().unwrap_or(0) as u32,
        w_physical_bytes: value["elem_bytes"].as_i64().unwrap_or(4) as i16,
        p_data: ptr::null_mut(), // SDK does not need the array for Set
    })
}

/// Build an NkMAIDRange from the JSON that `decode_value` produced for DT_RANGE_PTR.
/// Returns `Err(InvalidValue)` if `value` is absent or not numeric.
fn range_write_data(value: &Value) -> Result<NkMAIDRange, SdkError> {
    Ok(NkMAIDRange {
        lf_value: value["value"].as_f64().ok_or(SdkError::InvalidValue)?,
        lf_default: value["default"].as_f64().unwrap_or(0.0),
        ul_value_index: value["value_index"].as_u64().unwrap_or(0) as u32,
        ul_default_index: value["default_index"].as_u64().unwrap_or(0) as u32,
        lf_lower: value["lower"].as_f64().unwrap_or(0.0),
        lf_upper: value["upper"].as_f64().unwrap_or(0.0),
        ul_steps: value["steps"].as_u64().unwrap_or(0) as u32,
    })
}

/// Marshal a JSON value back to the camera via SetCapability.
///
/// Dispatches on `cap_kind` (kNkMAIDCapType_*) to select the right data type
/// and struct layout. For Enum and Range, `value` must be the JSON that
/// `decode_value` produced (containing the full struct fields needed for Set).
unsafe fn write_cap_value(
    sdk: &Sdk,
    capability_id: u32,
    cap_kind: u32,
    value: &Value,
) -> Result<(), SdkError> {
    match cap_kind {
        CAP_TYPE_BOOLEAN => {
            let b: u8 = if value.as_bool().ok_or(SdkError::InvalidValue)? { 1 } else { 0 };
            check("SetCapability", unsafe {
                (sdk.set_capability)(capability_id, &b as *const _ as *const c_void, DT_BOOLEAN_PTR)
            })
        }
        CAP_TYPE_INTEGER => {
            let v = value.as_i64().ok_or(SdkError::InvalidValue)? as i32;
            check("SetCapability", unsafe {
                (sdk.set_capability)(capability_id, &v as *const _ as *const c_void, DT_INTEGER_PTR)
            })
        }
        CAP_TYPE_UNSIGNED => {
            let v = value.as_u64().ok_or(SdkError::InvalidValue)? as u32;
            check("SetCapability", unsafe {
                (sdk.set_capability)(capability_id, &v as *const _ as *const c_void, DT_UNSIGNED_PTR)
            })
        }
        CAP_TYPE_FLOAT => {
            let v = value.as_f64().ok_or(SdkError::InvalidValue)?;
            check("SetCapability", unsafe {
                (sdk.set_capability)(capability_id, &v as *const _ as *const c_void, DT_FLOAT_PTR)
            })
        }
        CAP_TYPE_STRING => {
            let s = value.as_str().ok_or(SdkError::InvalidValue)?;
            let mut buf = [0u8; 256];
            let n = s.len().min(255);
            buf[..n].copy_from_slice(&s.as_bytes()[..n]);
            check("SetCapability", unsafe {
                (sdk.set_capability)(capability_id, buf.as_ptr() as *const c_void, DT_STRING_PTR)
            })
        }
        CAP_TYPE_ENUM => {
            let st = enum_write_data(value)?;
            check("SetCapability", unsafe {
                (sdk.set_capability)(capability_id, &st as *const _ as *const c_void, DT_ENUM_PTR)
            })
        }
        CAP_TYPE_RANGE => {
            let st = range_write_data(value)?;
            check("SetCapability", unsafe {
                (sdk.set_capability)(capability_id, &st as *const _ as *const c_void, DT_RANGE_PTR)
            })
        }
        other => Err(SdkError::UnsupportedWrite(other)),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── bcd_decode_version ───────────────────────────────────────────────

    #[test]
    fn bcd_decode_doc_examples() {
        assert_eq!(bcd_decode_version(0x0531), "5.31");
        assert_eq!(bcd_decode_version(0x0143), "1.43");
        assert_eq!(bcd_decode_version(0x0200), "2.00");
    }

    #[test]
    fn bcd_decode_two_digit_major() {
        // The inline rusb formula (v.0 as integer) gives "16.00" for 0x1000;
        // BCD decoding gives "10.00". This is the divergence point.
        assert_eq!(bcd_decode_version(0x1000), "10.00");
        assert_eq!(bcd_decode_version(0x1231), "12.31");
    }

    #[test]
    fn bcd_decode_single_digit_boundary() {
        assert_eq!(bcd_decode_version(0x0900), "9.00"); // last single-digit major
        assert_eq!(bcd_decode_version(0x0000), "0.00");
    }

    // ── pair_devices ─────────────────────────────────────────────────────

    fn dev(id: u32, name: &str) -> DeviceInfo {
        DeviceInfo { id, name: name.into(), available: true, connected_pid: 0, version: String::new() }
    }
    fn usb(model: &str, serial: &str) -> UsbCameraInfo {
        UsbCameraInfo { product_id: 0, serial: serial.into(), firmware: String::new(), model: model.into() }
    }

    #[test]
    fn pair_single_device() {
        let pairs = pair_devices(vec![dev(1, "Z 9")], &[usb("Z 9", "SER001")]);
        assert_eq!(pairs[0].1.as_ref().unwrap().serial, "SER001");
    }

    #[test]
    fn pair_two_same_model_preserves_order() {
        let pairs = pair_devices(
            vec![dev(1, "Z 9"), dev(2, "Z 9")],
            &[usb("Z 9", "FIRST"), usb("Z 9", "SECOND")],
        );
        assert_eq!(pairs[0].1.as_ref().unwrap().serial, "FIRST");
        assert_eq!(pairs[1].1.as_ref().unwrap().serial, "SECOND");
    }

    #[test]
    fn pair_more_sdk_than_usb_gives_none() {
        let pairs = pair_devices(
            vec![dev(1, "Z 9"), dev(2, "Z 9")],
            &[usb("Z 9", "ONLY")],
        );
        assert_eq!(pairs[0].1.as_ref().unwrap().serial, "ONLY");
        assert!(pairs[1].1.is_none());
    }

    #[test]
    fn pair_mixed_models_interleaved() {
        // SDK list: Z9, Z6III, Z9 — USB list has all three; Z9s must not cross-contaminate.
        let pairs = pair_devices(
            vec![dev(1, "Z 9"), dev(2, "Z 6III"), dev(3, "Z 9")],
            &[usb("Z 9", "Z9_A"), usb("Z 6III", "Z6_A"), usb("Z 9", "Z9_B")],
        );
        assert_eq!(pairs[0].1.as_ref().unwrap().serial, "Z9_A");
        assert_eq!(pairs[1].1.as_ref().unwrap().serial, "Z6_A");
        assert_eq!(pairs[2].1.as_ref().unwrap().serial, "Z9_B");
    }

    #[test]
    fn pair_no_usb_gives_all_none() {
        let pairs = pair_devices(vec![dev(1, "Z 9")], &[]);
        assert!(pairs[0].1.is_none());
    }

    // ── enum_write_data ───────────────────────────────────────────────────

    #[test]
    fn enum_write_missing_value_index_is_invalid() {
        let v = json!({"elem_type": 2, "elem_count": 5, "elem_bytes": 4});
        assert!(matches!(enum_write_data(&v), Err(SdkError::InvalidValue)));
    }

    #[test]
    fn enum_write_null_value_index_is_invalid() {
        let v = json!({"value_index": null, "elem_type": 2, "elem_count": 5, "elem_bytes": 4});
        assert!(matches!(enum_write_data(&v), Err(SdkError::InvalidValue)));
    }

    #[test]
    fn enum_write_extracts_all_fields() {
        let v = json!({
            "value_index": 3u64,
            "default_index": 1u64,
            "elem_count": 7u64,
            "elem_type": 2u64,
            "elem_bytes": 4i64,
            "values": [0, 1, 2, 3, 4, 5, 6],
        });
        let st = enum_write_data(&v).unwrap();
        assert_eq!(st.ul_value, 3);
        assert_eq!(st.ul_default, 1);
        assert_eq!(st.ul_elements, 7);
        assert_eq!(st.ul_type, 2);
        assert_eq!(st.w_physical_bytes, 4);
        assert!(st.p_data.is_null()); // never passed to Set
    }

    #[test]
    fn enum_write_missing_optional_fields_use_defaults() {
        // Only value_index is required; everything else has a fallback.
        let v = json!({"value_index": 0u64});
        let st = enum_write_data(&v).unwrap();
        assert_eq!(st.ul_value, 0);
        assert_eq!(st.ul_type, 2);   // default elem_type
        assert_eq!(st.w_physical_bytes, 4); // default elem_bytes
    }

    // ── range_write_data ──────────────────────────────────────────────────

    #[test]
    fn range_write_missing_value_is_invalid() {
        let v = json!({"lower": 1.0, "upper": 10.0, "steps": 0u64});
        assert!(matches!(range_write_data(&v), Err(SdkError::InvalidValue)));
    }

    #[test]
    fn range_write_null_value_is_invalid() {
        let v = json!({"value": null, "lower": 1.0, "upper": 10.0});
        assert!(matches!(range_write_data(&v), Err(SdkError::InvalidValue)));
    }

    #[test]
    fn range_write_extracts_all_fields() {
        let v = json!({
            "value": 3.5f64,
            "default": 0.0f64,
            "value_index": 7u64,
            "default_index": 0u64,
            "lower": 1.0f64,
            "upper": 10.0f64,
            "steps": 20u64,
        });
        let st = range_write_data(&v).unwrap();
        assert_eq!(st.lf_value, 3.5);
        assert_eq!(st.ul_value_index, 7);
        assert_eq!(st.ul_steps, 20);
        assert_eq!(st.lf_lower, 1.0);
        assert_eq!(st.lf_upper, 10.0);
    }

    #[test]
    fn range_write_missing_optional_fields_use_defaults() {
        let v = json!({"value": 5.0f64});
        let st = range_write_data(&v).unwrap();
        assert_eq!(st.lf_value, 5.0);
        assert_eq!(st.lf_default, 0.0);
        assert_eq!(st.ul_steps, 0);
    }
}
