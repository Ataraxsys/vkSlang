//! vkSlang — Vulkan layer running libretro `.slangp` presets through
//! librashader on the application's swapchain.
//!
//! Loader entry points: `vkNegotiateLoaderLayerInterfaceVersion`,
//! `vkGetInstanceProcAddr`, `vkGetDeviceProcAddr`.

#![allow(clippy::missing_safety_doc)]

mod config;
mod hooks;
mod loader;
mod log;
mod render;
mod state;

use ash::vk;
use loader::{NegotiateLayerInterface, NegotiateLayerStructType, CURRENT_LOADER_LAYER_INTERFACE_VERSION};
use std::ffi::{c_char, CStr};

/// Casts a hook to the generic `PFN_vkVoidFunction`.
macro_rules! pfn {
    ($f:expr) => {
        Some(std::mem::transmute::<*const (), unsafe extern "system" fn()>($f as *const ()))
    };
}

/// Device-level hooks, also returned by vkGetInstanceProcAddr as required
/// for applications resolving device functions through the instance.
unsafe fn device_hook(name: &CStr) -> vk::PFN_vkVoidFunction {
    match name.to_bytes() {
        b"vkGetDeviceProcAddr" => pfn!(vkGetDeviceProcAddr),
        b"vkDestroyDevice" => pfn!(hooks::destroy_device),
        b"vkGetDeviceQueue" => pfn!(hooks::get_device_queue),
        b"vkGetDeviceQueue2" => pfn!(hooks::get_device_queue2),
        b"vkCreateSwapchainKHR" => pfn!(hooks::create_swapchain),
        b"vkDestroySwapchainKHR" => pfn!(hooks::destroy_swapchain),
        b"vkQueuePresentKHR" => pfn!(hooks::queue_present),
        _ => None,
    }
}

unsafe fn instance_hook(name: &CStr) -> vk::PFN_vkVoidFunction {
    match name.to_bytes() {
        b"vkGetInstanceProcAddr" => pfn!(vkGetInstanceProcAddr),
        b"vkCreateInstance" => pfn!(hooks::create_instance),
        b"vkDestroyInstance" => pfn!(hooks::destroy_instance),
        b"vkCreateDevice" => pfn!(hooks::create_device),
        _ => None,
    }
}

#[no_mangle]
pub unsafe extern "system" fn vkGetInstanceProcAddr(
    instance: vk::Instance,
    p_name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    let name = CStr::from_ptr(p_name);
    if let Some(f) = instance_hook(name).or_else(|| device_hook(name)) {
        return Some(f);
    }
    if instance == vk::Instance::null() {
        return None;
    }
    let data = state::instance(instance)?;
    (data.next_gipa)(instance, p_name)
}

#[no_mangle]
pub unsafe extern "system" fn vkGetDeviceProcAddr(
    device: vk::Device,
    p_name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    let name = CStr::from_ptr(p_name);
    if let Some(f) = device_hook(name) {
        return Some(f);
    }
    if device == vk::Device::null() {
        return None;
    }
    let data = state::device(device)?;
    (data.next_gdpa)(device, p_name)
}

#[no_mangle]
pub unsafe extern "system" fn vkNegotiateLoaderLayerInterfaceVersion(
    p_version_struct: *mut NegotiateLayerInterface,
) -> vk::Result {
    let Some(v) = p_version_struct.as_mut() else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    if v.s_type != NegotiateLayerStructType::InterfaceStruct || v.loader_layer_interface_version < 2 {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    v.loader_layer_interface_version = v.loader_layer_interface_version.min(CURRENT_LOADER_LAYER_INTERFACE_VERSION);
    v.pfn_get_instance_proc_addr = Some(vkGetInstanceProcAddr);
    v.pfn_get_device_proc_addr = Some(vkGetDeviceProcAddr);
    v.pfn_get_physical_device_proc_addr = None;
    vk::Result::SUCCESS
}
