//! Loader <-> layer interface structures from `vulkan/vk_layer.h`.
//!
//! ash only exposes the `sType` values (`LOADER_INSTANCE_CREATE_INFO`,
//! `LOADER_DEVICE_CREATE_INFO`), so the chain structures are mirrored here.

use ash::vk;
use std::ffi::{c_char, c_void};

pub const CURRENT_LOADER_LAYER_INTERFACE_VERSION: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NegotiateLayerStructType {
    Uninitialized = 0,
    InterfaceStruct = 1,
}

pub type PfnGetPhysicalDeviceProcAddr =
    unsafe extern "system" fn(vk::Instance, *const c_char) -> vk::PFN_vkVoidFunction;

#[repr(C)]
pub struct NegotiateLayerInterface {
    pub s_type: NegotiateLayerStructType,
    pub p_next: *mut c_void,
    pub loader_layer_interface_version: u32,
    pub pfn_get_instance_proc_addr: Option<vk::PFN_vkGetInstanceProcAddr>,
    pub pfn_get_device_proc_addr: Option<vk::PFN_vkGetDeviceProcAddr>,
    pub pfn_get_physical_device_proc_addr: Option<PfnGetPhysicalDeviceProcAddr>,
}

/// `VkLayerFunction`
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)]
pub enum LayerFunction {
    LayerLinkInfo = 0,
    LoaderDataCallback = 1,
    LoaderLayerCreateDeviceCallback = 2,
    LoaderFeatures = 3,
}

#[repr(C)]
pub struct LayerInstanceLink {
    pub p_next: *mut LayerInstanceLink,
    pub pfn_next_get_instance_proc_addr: vk::PFN_vkGetInstanceProcAddr,
    pub pfn_next_get_physical_device_proc_addr: Option<PfnGetPhysicalDeviceProcAddr>,
}

#[repr(C)]
pub struct LayerInstanceCreateInfo {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    /// Raw `VkLayerFunction`; kept as u32 so unknown future values are not UB.
    pub function: u32,
    /// Union; only `pLayerInfo` is used here. Sized for the largest member
    /// (`layerDevice`: two function pointers).
    pub u: [*mut c_void; 2],
}

#[repr(C)]
pub struct LayerDeviceLink {
    pub p_next: *mut LayerDeviceLink,
    pub pfn_next_get_instance_proc_addr: vk::PFN_vkGetInstanceProcAddr,
    pub pfn_next_get_device_proc_addr: vk::PFN_vkGetDeviceProcAddr,
}

pub type PfnSetDeviceLoaderData = unsafe extern "system" fn(vk::Device, *mut c_void) -> vk::Result;

#[repr(C)]
pub struct LayerDeviceCreateInfo {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    /// Raw `VkLayerFunction`; kept as u32 so unknown future values are not UB.
    pub function: u32,
    /// Union { VkLayerDeviceLink* pLayerInfo; PFN_vkSetDeviceLoaderData pfnSetDeviceLoaderData; }
    pub u: *mut c_void,
}

/// Walks a `pNext` chain looking for the loader structure of `s_type`
/// carrying `function`.
unsafe fn find_in_chain<T>(
    mut p: *const c_void,
    s_type: vk::StructureType,
    function: LayerFunction,
    get: impl Fn(*mut T) -> (vk::StructureType, u32),
) -> Option<*mut T> {
    while !p.is_null() {
        let (st, func) = get(p as *mut T);
        if st == s_type && func == function as u32 {
            return Some(p as *mut T);
        }
        p = (*(p as *const vk::BaseInStructure)).p_next as *const c_void;
    }
    None
}

pub unsafe fn instance_chain_info(
    info: &vk::InstanceCreateInfo,
    function: LayerFunction,
) -> Option<*mut LayerInstanceCreateInfo> {
    find_in_chain::<LayerInstanceCreateInfo>(
        info.p_next,
        vk::StructureType::LOADER_INSTANCE_CREATE_INFO,
        function,
        |p| ((*p).s_type, (*p).function),
    )
}

pub unsafe fn device_chain_info(
    info: &vk::DeviceCreateInfo,
    function: LayerFunction,
) -> Option<*mut LayerDeviceCreateInfo> {
    find_in_chain::<LayerDeviceCreateInfo>(
        info.p_next,
        vk::StructureType::LOADER_DEVICE_CREATE_INFO,
        function,
        |p| ((*p).s_type, (*p).function),
    )
}
