//! The loader ↔ layer interface (`vk_layer.h`), which ash doesn't carry.

use std::ffi::{CStr, c_char, c_void};

use ash::vk;

/// `VkLayerFunction`.
pub const LAYER_LINK_INFO: i32 = 0;
pub const LOADER_DATA_CALLBACK: i32 = 1;

/// `VkNegotiateLayerStructType::LAYER_NEGOTIATE_INTERFACE_STRUCT`.
pub const LAYER_NEGOTIATE_INTERFACE_STRUCT: i32 = 1;
/// The loader/layer interface version this layer speaks.
pub const LAYER_INTERFACE_VERSION: u32 = 2;

pub type PfnGetPhysicalDeviceProcAddr = Option<unsafe extern "system" fn(vk::Instance, *const c_char) -> vk::PFN_vkVoidFunction>;
pub type PfnSetDeviceLoaderData = unsafe extern "system" fn(vk::Device, *mut c_void) -> vk::Result;

#[repr(C)]
pub struct LayerInstanceLink {
    pub next: *mut LayerInstanceLink,
    pub next_get_instance_proc_addr: Option<vk::PFN_vkGetInstanceProcAddr>,
    pub next_get_physical_device_proc_addr: PfnGetPhysicalDeviceProcAddr,
}

#[repr(C)]
pub union LayerInstanceUnion {
    pub layer_info: *mut LayerInstanceLink,
    pub set_instance_loader_data: *const c_void,
    pub layer_device: [*const c_void; 2],
    pub loader_features: u32,
}

/// `VkLayerInstanceCreateInfo`.
#[repr(C)]
pub struct LayerInstanceCreateInfo {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub function: i32,
    pub u: LayerInstanceUnion,
}

#[repr(C)]
pub struct LayerDeviceLink {
    pub next: *mut LayerDeviceLink,
    pub next_get_instance_proc_addr: Option<vk::PFN_vkGetInstanceProcAddr>,
    pub next_get_device_proc_addr: Option<vk::PFN_vkGetDeviceProcAddr>,
}

#[repr(C)]
pub union LayerDeviceUnion {
    pub layer_info: *mut LayerDeviceLink,
    pub set_device_loader_data: Option<PfnSetDeviceLoaderData>,
}

/// `VkLayerDeviceCreateInfo`.
#[repr(C)]
pub struct LayerDeviceCreateInfo {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub function: i32,
    pub u: LayerDeviceUnion,
}

/// `VkNegotiateLayerInterface`.
#[repr(C)]
pub struct NegotiateLayerInterface {
    pub s_type: i32,
    pub p_next: *mut c_void,
    pub loader_layer_interface_version: u32,
    pub get_instance_proc_addr: Option<vk::PFN_vkGetInstanceProcAddr>,
    pub get_device_proc_addr: Option<vk::PFN_vkGetDeviceProcAddr>,
    pub get_physical_device_proc_addr: PfnGetPhysicalDeviceProcAddr,
}

/// The generic `VkBaseInStructure` view of a `pNext` chain element.
#[repr(C)]
struct BaseIn {
    s_type: vk::StructureType,
    p_next: *const BaseIn,
}

/// Finds the first structure of `s_type` in a `pNext` chain for which `pred` holds.
///
/// # Safety
/// `p_next` must be a valid Vulkan `pNext` chain.
pub unsafe fn find_in_chain(mut p_next: *const c_void, s_type: vk::StructureType, mut pred: impl FnMut(*const c_void) -> bool) -> *const c_void {
    let mut guard = 0;
    while !p_next.is_null() && guard < 64 {
        let base = p_next as *const BaseIn;
        // SAFETY: the caller's chain is valid.
        if unsafe { (*base).s_type } == s_type && pred(p_next) {
            return p_next;
        }
        p_next = unsafe { (*base).p_next } as *const c_void;
        guard += 1;
    }
    std::ptr::null()
}

/// The dispatch key of a dispatchable handle: the loader's dispatch table pointer stored at
/// its start. Every queue and command buffer of a device shares the device's key.
///
/// # Safety
/// `raw` must be a valid dispatchable handle.
#[inline]
pub unsafe fn dispatch_key(raw: u64) -> usize {
    // SAFETY: dispatchable handles point at an object whose first word is the dispatch table.
    unsafe { *(raw as usize as *const usize) }
}

/// A C string from the application, cut into a fixed field (lossy, untrusted).
///
/// # Safety
/// `p` is null or a NUL-terminated string.
pub unsafe fn c_str<'a>(p: *const c_char) -> Option<&'a CStr> {
    // SAFETY: as documented.
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) })
}
