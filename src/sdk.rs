//! Nikon Remote SDK v2 — FFI and safe Rust wrapper.
//!
//! This is the only module in nikon-fleet that contains `unsafe`. The
//! intent is to keep the surface area of `unsafe` as small and well-named
//! as possible; everything outside this file is plain safe Rust.
//!
//! ## Lifecycle
//!
//! ```text
//!   Sdk::open(bundle_path)         // dlopen the bundle, resolve symbols
//!     .initialize()                // InitializeSDK, returns first device list
//!   sdk.devices()                  // EnumDevices, returns Vec<DeviceInfo>
//!   sdk.connect(device_id)         // ConnectDevice, returns capability list
//!     .read_capability(cap_id)     // GetCapability(Value) → JSON
//!   (drop the Device → DisconnectDevice)
//!   (drop the Sdk → FreeSDK)
//! ```
//!
//! ## Threading
//!
//! The SDK is not documented to be thread-safe. We're single-threaded
//! everywhere in this codebase; if we ever need concurrency around it,
//! gate calls behind a Mutex at this layer.
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

// ─────────────────────────────────────────────────────────────────────────
// C types — must mirror Maid3.h exactly. #[repr(C)] guarantees field order
// and ABI-compatible layout.
// ─────────────────────────────────────────────────────────────────────────

/// `NkMAIDDeviceInfo` from Maid3.h.
///
/// Layout discipline: u32 (4) + char[64] + C bool (1 byte) + 3 padding +
/// u32 (4) + char[64] = 140 bytes. We let Rust compute the padding via
/// the standard struct rules under `#[repr(C)]`.
#[repr(C)]
struct NkMAIDDeviceInfo {
    id: u32,
    name: [c_char; 64],
    availability: bool,
    // 3 bytes of padding implicitly here under repr(C).
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

/// Callback registration struct — InitializeSDK wants this. We provide
/// no-op callbacks since snapshotting doesn't need them.
#[repr(C)]
struct NkMAIDCSCallback {
    p_ui_req_proc: *mut c_void,
    p_event_proc: *mut c_void,
    p_progress_proc: *mut c_void,
    p_data_proc: *mut c_void,
    p_live_view_data_proc: *mut c_void,
    ref_proc: *mut c_void,
}

// eNkMAIDDataType — the discriminator the SDK returns alongside a value
// pointer to tell us how to interpret it.
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

// eNkSDKGetSettingRequestType
const GET_VALUE: u32 = 0;

// ─────────────────────────────────────────────────────────────────────────
// Function pointer signatures. We trust Maid3.h here. CALLPASCAL is a
// no-op on macOS so plain `extern "C"` is correct.
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
// Allocator callbacks the SDK uses to allocate buffers it returns to us.
// We hand back the same allocator for freeing.
// ─────────────────────────────────────────────────────────────────────────

unsafe extern "C" fn sdk_alloc(size: libc::size_t) -> *mut c_void {
    unsafe { libc::malloc(size) }
}

unsafe extern "C" fn sdk_free(ptr: *mut c_void) {
    unsafe { libc::free(ptr) }
}

// Stub callbacks. The SDK appears to validate that these are non-null even
// though we don't actually need event/UI/progress/data delivery for a
// snapshot-only workflow. Their exact signatures vary, but since we don't
// expect them to be invoked in this code path, a no-op extern "C" works.
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
    // Per Maid3.h: negative values are errors, non-negative are success
    // (some functions return 0 or a positive HRESULT-style success code).
    if code < 0 {
        Err(SdkError::SdkCall { call, code })
    } else {
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Sdk — the loaded library + initialized session.
// ─────────────────────────────────────────────────────────────────────────

/// Resolved SDK entry points. Created with `Sdk::open`, kept alive for the
/// duration of any camera I/O. Dropping it calls FreeSDK.
pub struct Sdk {
    // Field order matters for Drop: function pointers (which borrow from
    // `_lib`) must drop before the Library does. Rust drops fields in
    // declaration order, so list these BEFORE `_lib`.
    initialize: FnInitializeSDK,
    free_sdk: FnFreeSDK,
    enum_devices: FnEnumDevices,
    connect_device: FnConnectDevice,
    disconnect_device: FnDisconnectDevice,
    get_capability: FnGetCapability,
    initialized: bool,
    _lib: Library, // last — outlives the function pointers
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
        // SAFETY: dlopen has global side effects. We trust the user-given path.
        let lib = unsafe { Library::new(&path) }
            .map_err(|e| SdkError::Load { path: path.clone(), source: e })?;

        // Helper: resolve a symbol by name into a typed function pointer.
        // We immediately cast away the lifetime tying it to `lib`; the
        // Sdk struct keeps `lib` alive for as long as the function pointers
        // are used (field-drop-order discipline above).
        unsafe fn resolve<T: Copy>(lib: &Library, name: &[u8]) -> Result<T, SdkError> {
            let sym: Symbol<T> = unsafe { lib.get(name) }
                .map_err(|e| SdkError::Symbol(String::from_utf8_lossy(name).into_owned(), e))?;
            Ok(*sym)
        }

        unsafe {
            let initialize = resolve(&lib, b"InitializeSDK\0")?;
            let free_sdk = resolve(&lib, b"FreeSDK\0")?;
            let enum_devices = resolve(&lib, b"EnumDevices\0")?;
            let connect_device = resolve(&lib, b"ConnectDevice\0")?;
            let disconnect_device = resolve(&lib, b"DisconnectDevice\0")?;
            let get_capability = resolve(&lib, b"GetCapability\0")?;

            Ok(Sdk {
                initialize,
                free_sdk,
                enum_devices,
                connect_device,
                disconnect_device,
                get_capability,
                initialized: false,
                _lib: lib,
            })
        }
    }

    /// Calls InitializeSDK. Required before any other call.
    pub fn initialize(&mut self) -> Result<(), SdkError> {
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
        // We pass NULL for ppEnumCapInfo here — that argument is only valid
        // once we've connected to a device. The sample app does the same;
        // passing a non-null address triggers InvalidArguments (-93).
        // SAFETY: all pointers are valid; allocator callbacks are valid extern "C" fns.
        let code = unsafe {
            (self.initialize)(
                sdk_alloc,
                sdk_free,
                &callback,
                &mut device_list,
                ptr::null_mut(),
            )
        };
        check("InitializeSDK", code)?;
        self.initialized = true;
        // The SDK allocated buffers for device_list; we'll re-query via
        // EnumDevices on demand. Free what was returned here.
        if !device_list.is_null() {
            unsafe {
                let dl = &*device_list;
                if !dl.p_device_data.is_null() {
                    sdk_free(dl.p_device_data as *mut c_void);
                }
                sdk_free(device_list as *mut c_void);
            }
        }
        Ok(())
    }

    /// Enumerate USB-attached devices.
    pub fn devices(&self) -> Result<Vec<DeviceInfo>, SdkError> {
        let mut list: *mut NkMAIDEnumDevices = ptr::null_mut();
        // SAFETY: SDK fills `list` to a heap-allocated buffer; we own it
        // until we explicitly free.
        let code = unsafe {
            (self.enum_devices)(&mut list, ptr::null_mut(), ptr::null_mut())
        };
        check("EnumDevices", code)?;
        if list.is_null() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
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
            if !dl.p_device_data.is_null() {
                sdk_free(dl.p_device_data as *mut c_void);
            }
            sdk_free(list as *mut c_void);
        }
        Ok(out)
    }

    /// Open a session on the given device. Returns a `Device` that holds
    /// the connection. Dropping it disconnects.
    pub fn connect(&mut self, device_id: u32) -> Result<Device<'_>, SdkError> {
        let mut cap_info: *mut NkMAIDEnumCapInfo = ptr::null_mut();
        // SAFETY: SDK fills cap_info to a heap-allocated buffer.
        let code = unsafe { (self.connect_device)(device_id, &mut cap_info) };
        check("ConnectDevice", code)?;
        let capabilities = unsafe { take_capabilities(cap_info) };
        Ok(Device { sdk: self, capabilities })
    }
}

impl Drop for Sdk {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: FreeSDK is the documented cleanup; ignore the result code.
            let _ = unsafe { (self.free_sdk)() };
        }
    }
}

/// An active camera session. Disconnects on drop.
pub struct Device<'sdk> {
    sdk: &'sdk Sdk,
    pub capabilities: Vec<CapabilityInfo>,
}

impl<'sdk> Device<'sdk> {
    /// Read the current value of one capability. Returns a JSON-compatible
    /// value when the data type is one we know how to decode; otherwise
    /// returns a tagged object describing the unrecognized type.
    pub fn read_capability(&self, capability_id: u32) -> Result<Value, SdkError> {
        let mut data_ptr: *mut c_void = ptr::null_mut();
        let mut data_type: u32 = 0;
        // SAFETY: SDK fills data_ptr with either a primitive cast to *mut c_void
        // (for by-value types) or a heap-allocated buffer we must free.
        let code = unsafe {
            (self.sdk.get_capability)(
                capability_id,
                GET_VALUE,
                &mut data_ptr,
                &mut data_type,
            )
        };
        check("GetCapability", code)?;
        let value = unsafe { decode_value(data_ptr, data_type) };
        // For pointer-typed values, free the SDK's buffer after decoding.
        if data_type >= DT_BOOLEAN_PTR && !data_ptr.is_null() {
            unsafe { sdk_free(data_ptr) };
        }
        Ok(value)
    }
}

impl<'sdk> Drop for Device<'sdk> {
    fn drop(&mut self) {
        // SAFETY: SDK contract — Disconnect is idempotent-enough; ignore code.
        let _ = unsafe { (self.sdk.disconnect_device)() };
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────

/// Convert a fixed-length C char array to a Rust String, stopping at the
/// first NUL.
fn cstr_to_string(buf: &[c_char]) -> String {
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len()) };
    let nul = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..nul]).into_owned()
}

/// Pull the capability list out of an EnumCapInfo* the SDK allocated, then
/// free its backing buffers.
unsafe fn take_capabilities(p: *mut NkMAIDEnumCapInfo) -> Vec<CapabilityInfo> {
    if p.is_null() {
        return Vec::new();
    }
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
        if !info.p_cap_array.is_null() {
            sdk_free(info.p_cap_array as *mut c_void);
        }
        sdk_free(p as *mut c_void);
    }
    out
}

/// Decode a (data_ptr, data_type) pair from GetCapability into JSON.
///
/// For by-value types (Integer, Unsigned, Boolean), the SDK stuffs the
/// value directly into the pointer width — we cast accordingly.
/// For pointer types, the pointer is to a heap buffer of the indicated type.
unsafe fn decode_value(data_ptr: *mut c_void, data_type: u32) -> Value {
    match data_type {
        DT_NULL => Value::Null,
        DT_INTEGER => json!(data_ptr as i64 as i32),
        DT_UNSIGNED => json!(data_ptr as usize as u32),
        DT_BOOLEAN_PTR => {
            if data_ptr.is_null() {
                Value::Null
            } else {
                let b = unsafe { *(data_ptr as *const u8) };
                json!(b != 0)
            }
        }
        DT_INTEGER_PTR => {
            if data_ptr.is_null() {
                Value::Null
            } else {
                let i = unsafe { *(data_ptr as *const i32) };
                json!(i)
            }
        }
        DT_UNSIGNED_PTR => {
            if data_ptr.is_null() {
                Value::Null
            } else {
                let u = unsafe { *(data_ptr as *const u32) };
                json!(u)
            }
        }
        DT_FLOAT_PTR => {
            if data_ptr.is_null() {
                Value::Null
            } else {
                let f = unsafe { *(data_ptr as *const f64) };
                json!(f)
            }
        }
        DT_STRING_PTR => {
            // NkMAIDString has a length field + char buffer; for now treat
            // as a C string from the buffer. Refine if we hit garbled output.
            if data_ptr.is_null() {
                Value::Null
            } else {
                let cs = unsafe { CStr::from_ptr(data_ptr as *const c_char) };
                json!(cs.to_string_lossy().into_owned())
            }
        }
        DT_RANGE_PTR | DT_ENUM_PTR => {
            // Decoding these requires struct definitions we haven't pulled in
            // yet. Surface the raw type for now so we can see in snapshot output
            // which properties need attention.
            json!({ "_unsupported_type": data_type })
        }
        _ => json!({ "_unknown_type": data_type }),
    }
}
