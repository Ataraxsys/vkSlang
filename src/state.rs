//! Global layer state, keyed by loader dispatch key.
//!
//! Every dispatchable handle (VkInstance, VkPhysicalDevice, VkDevice, VkQueue,
//! VkCommandBuffer) starts with a pointer to the loader dispatch table. All
//! children of an instance/device share that pointer, so it is used as the map
//! key, exactly like vkBasalt's `GetKey()`.

use crate::loader::PfnSetDeviceLoaderData;
use crate::render::Runtime;
use ash::vk;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

pub type DispatchKey = usize;

/// # Safety
/// `handle` must be a valid dispatchable Vulkan handle.
pub unsafe fn dispatch_key<H: vk::Handle>(handle: H) -> DispatchKey {
    *(handle.as_raw() as usize as *const usize)
}

pub struct InstanceData {
    pub handle: vk::Instance,
    pub next_gipa: vk::PFN_vkGetInstanceProcAddr,
    /// Instance functions resolved through the next layer.
    pub fns: ash::Instance,
    pub surface_fn: ash::khr::surface::InstanceFn,
}

pub struct DeviceData {
    pub handle: vk::Device,
    pub instance: Arc<InstanceData>,
    pub physical_device: vk::PhysicalDevice,
    pub next_gdpa: vk::PFN_vkGetDeviceProcAddr,
    /// Device functions resolved through the next layer (handed to librashader).
    pub fns: ash::Device,
    pub swapchain_fn: ash::khr::swapchain::DeviceFn,
    pub set_loader_data: PfnSetDeviceLoaderData,
    /// Graphics queue owned by the layer (fallback when presenting from a
    /// queue of another family).
    pub queue: vk::Queue,
    pub queue_family: u32,
    /// Whether VK_KHR_swapchain_mutable_format was enabled on the device.
    pub mutable_format: bool,
    /// False when the preset is missing, the process is filtered out, or no
    /// graphics queue exists: every hook then forwards untouched.
    pub active: bool,
    /// VkQueue -> queue family, filled by vkGetDeviceQueue(2).
    pub queue_families: Mutex<HashMap<vk::Queue, u32>>,
    /// Command pool, frame ring, filter chain and swapchains.
    pub runtime: Mutex<Option<Runtime>>,
}

pub static INSTANCES: LazyLock<Mutex<HashMap<DispatchKey, Arc<InstanceData>>>> =
    LazyLock::new(Default::default);
pub static DEVICES: LazyLock<Mutex<HashMap<DispatchKey, Arc<DeviceData>>>> =
    LazyLock::new(Default::default);

pub fn instance<H: vk::Handle>(handle: H) -> Option<Arc<InstanceData>> {
    let key = unsafe { dispatch_key(handle) };
    INSTANCES.lock().unwrap().get(&key).cloned()
}

pub fn device<H: vk::Handle>(handle: H) -> Option<Arc<DeviceData>> {
    let key = unsafe { dispatch_key(handle) };
    DEVICES.lock().unwrap().get(&key).cloned()
}

/// Resolves `name` through `gipa` and casts it to the requested PFN type.
///
/// # Safety
/// `F` must be the function pointer type matching `name`.
pub unsafe fn load_pfn<F>(
    gipa: vk::PFN_vkGetInstanceProcAddr,
    instance: vk::Instance,
    name: &std::ffi::CStr,
) -> Option<F> {
    debug_assert_eq!(size_of::<F>(), size_of::<usize>());
    gipa(instance, name.as_ptr()).map(|f| std::mem::transmute_copy(&f))
}
