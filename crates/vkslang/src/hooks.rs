//! Intercepted Vulkan entry points.
//!
//! Each hook calls the next layer through pointers captured at create time
//! (never through the loader), mirroring vkBasalt's `vkBasalt_*` functions.

use crate::config::{self, HdrOutput};
use crate::loader::{self, LayerDeviceLink, LayerFunction, LayerInstanceLink, PfnSetDeviceLoaderData};
use crate::render::{srgb_to_unorm, Runtime, SwapchainPlan, SwapchainState};
use crate::state::{self, load_pfn, DeviceData, InstanceData, DEVICES, INSTANCES};
use crate::{log_debug, log_error, log_info, log_warn};
use ash::vk::{self, Handle};
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr};
use std::sync::{Arc, Mutex};

unsafe fn slice<'a, T>(ptr: *const T, len: u32) -> &'a [T] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(ptr, len as usize)
    }
}

// ---------------------------------------------------------------- instance

pub unsafe extern "system" fn create_instance(
    p_create_info: *const vk::InstanceCreateInfo,
    p_allocator: *const vk::AllocationCallbacks,
    p_instance: *mut vk::Instance,
) -> vk::Result {
    let Some(chain) = loader::instance_chain_info(&*p_create_info, LayerFunction::LayerLinkInfo) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let link = (*chain).u[0] as *mut LayerInstanceLink;
    if link.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    let next_gipa = (*link).pfn_next_get_instance_proc_addr;
    // Advance the link for the next layer before calling down.
    (*chain).u[0] = (*link).p_next as *mut c_void;

    let Some(next_create): Option<vk::PFN_vkCreateInstance> =
        load_pfn(next_gipa, vk::Instance::null(), c"vkCreateInstance")
    else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };

    // Enable VK_EXT_swapchain_colorspace even when the application does not:
    // without it the surface never reports the HDR10/scRGB formats the layer
    // needs to promote the swapchain to HDR. Availability is not queried (a
    // layer below us may not answer instance-level queries before the
    // instance exists), the call is simply retried without it on failure.
    let ci = &*p_create_info;
    let colorspace_ext = ash::ext::swapchain_colorspace::NAME;
    let already_enabled = slice(ci.pp_enabled_extension_names, ci.enabled_extension_count)
        .iter()
        .any(|&e| CStr::from_ptr(e) == colorspace_ext);
    let add_colorspace = config::get().hdr_output != HdrOutput::Off && !already_enabled;

    let mut extensions: Vec<*const c_char> =
        slice(ci.pp_enabled_extension_names, ci.enabled_extension_count).to_vec();
    let mut modified = *ci;
    if add_colorspace {
        extensions.push(colorspace_ext.as_ptr());
        modified.enabled_extension_count = extensions.len() as u32;
        modified.pp_enabled_extension_names = extensions.as_ptr();
    }

    let mut result = next_create(&modified, p_allocator, p_instance);
    if result != vk::Result::SUCCESS && add_colorspace {
        log_debug!("{} unavailable ({result}), creating the instance as asked", colorspace_ext.to_string_lossy());
        result = next_create(p_create_info, p_allocator, p_instance);
    }
    if result != vk::Result::SUCCESS {
        return result;
    }

    let handle = *p_instance;
    let load = |name: &CStr| std::mem::transmute::<_, *const c_void>(next_gipa(handle, name.as_ptr()));
    let data = InstanceData {
        handle,
        next_gipa,
        fns: ash::Instance::load_with(load, handle),
        surface_fn: ash::khr::surface::InstanceFn::load(load),
    };
    INSTANCES.lock().unwrap().insert(state::dispatch_key(handle), Arc::new(data));
    result
}

pub unsafe extern "system" fn destroy_instance(instance: vk::Instance, p_allocator: *const vk::AllocationCallbacks) {
    if instance == vk::Instance::null() {
        return;
    }
    let key = state::dispatch_key(instance);
    if let Some(data) = INSTANCES.lock().unwrap().remove(&key) {
        data.fns.destroy_instance(p_allocator.as_ref());
    }
}

// ------------------------------------------------------------------ device

fn has_ext(list: &[vk::ExtensionProperties], name: &CStr) -> bool {
    list.iter().any(|e| e.extension_name_as_c_str() == Ok(name))
}

pub unsafe extern "system" fn create_device(
    physical_device: vk::PhysicalDevice,
    p_create_info: *const vk::DeviceCreateInfo,
    p_allocator: *const vk::AllocationCallbacks,
    p_device: *mut vk::Device,
) -> vk::Result {
    // A VkPhysicalDevice shares its dispatch key with its VkInstance.
    let Some(inst) = state::instance(physical_device) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let ci = &*p_create_info;
    let Some(chain) = loader::device_chain_info(ci, LayerFunction::LayerLinkInfo) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let link = (*chain).u as *mut LayerDeviceLink;
    if link.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    let next_gipa = (*link).pfn_next_get_instance_proc_addr;
    let next_gdpa = (*link).pfn_next_get_device_proc_addr;
    (*chain).u = (*link).p_next as *mut c_void;

    let Some(set_loader_data) = loader::device_chain_info(ci, LayerFunction::LoaderDataCallback)
        .map(|c| std::mem::transmute::<*mut c_void, PfnSetDeviceLoaderData>((*c).u))
    else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let Some(next_create): Option<vk::PFN_vkCreateDevice> = load_pfn(next_gipa, inst.handle, c"vkCreateDevice")
    else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };

    let cfg = config::get();
    let wanted = cfg.preset.is_some() && cfg.active_for_process();

    // The layer renders on a graphics-capable queue the app asked for.
    let families = inst.fns.get_physical_device_queue_family_properties(physical_device);
    let graphics_family = slice(ci.p_queue_create_infos, ci.queue_create_info_count)
        .iter()
        .map(|q| q.queue_family_index)
        .find(|&f| {
            families
                .get(f as usize)
                .is_some_and(|p| p.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        });

    // Mutable swapchain format lets us render through a UNORM view of an sRGB
    // swapchain (no double gamma), as vkBasalt does.
    let mut extensions: Vec<*const c_char> =
        slice(ci.pp_enabled_extension_names, ci.enabled_extension_count).to_vec();
    let mut mutable_format = false;
    if wanted {
        let available = inst
            .fns
            .enumerate_device_extension_properties(physical_device)
            .unwrap_or_default();
        let needed = [
            ash::khr::swapchain_mutable_format::NAME,
            ash::khr::image_format_list::NAME,
            ash::khr::maintenance2::NAME,
        ];
        if needed.iter().all(|n| has_ext(&available, n)) {
            for n in needed {
                if !extensions.iter().any(|&e| CStr::from_ptr(e) == n) {
                    extensions.push(n.as_ptr());
                }
            }
            mutable_format = true;
        }
    }
    let mut modified = *ci;
    modified.enabled_extension_count = extensions.len() as u32;
    modified.pp_enabled_extension_names = extensions.as_ptr();

    let result = next_create(physical_device, &modified, p_allocator, p_device);
    if result != vk::Result::SUCCESS {
        return result;
    }

    let handle = *p_device;
    let load = |name: &CStr| std::mem::transmute::<_, *const c_void>(next_gdpa(handle, name.as_ptr()));
    let fns = ash::Device::load_with(load, handle);
    let swapchain_fn = ash::khr::swapchain::DeviceFn::load(load);

    let queue = match graphics_family {
        Some(family) => {
            let q = fns.get_device_queue(family, 0);
            let r = set_loader_data(handle, q.as_raw() as usize as *mut c_void);
            if r != vk::Result::SUCCESS {
                log_warn!("vkSetDeviceLoaderData on the layer queue failed: {r}");
            }
            q
        }
        None => vk::Queue::null(),
    };
    let active = wanted && graphics_family.is_some();
    if wanted && !active {
        log_warn!("no graphics queue requested by the application, layer inactive on this device");
    }
    if active {
        crate::ipc::start();
    }
    log_debug!("device created (active={active}, mutable_format={mutable_format})");

    let data = DeviceData {
        handle,
        instance: inst,
        physical_device,
        next_gdpa,
        fns,
        swapchain_fn,
        set_loader_data,
        queue,
        queue_family: graphics_family.unwrap_or(0),
        mutable_format,
        active,
        queue_families: Mutex::new(HashMap::new()),
        runtime: Mutex::new(None),
    };
    DEVICES.lock().unwrap().insert(state::dispatch_key(handle), Arc::new(data));
    result
}

pub unsafe extern "system" fn destroy_device(device: vk::Device, p_allocator: *const vk::AllocationCallbacks) {
    if device == vk::Device::null() {
        return;
    }
    let key = state::dispatch_key(device);
    let Some(dev) = DEVICES.lock().unwrap().remove(&key) else { return };
    if let Some(rt) = dev.runtime.lock().unwrap().take() {
        rt.destroy(&dev);
    }
    dev.fns.destroy_device(p_allocator.as_ref());
}

pub unsafe extern "system" fn get_device_queue(
    device: vk::Device,
    family: u32,
    index: u32,
    p_queue: *mut vk::Queue,
) {
    let Some(dev) = state::device(device) else { return };
    (dev.fns.fp_v1_0().get_device_queue)(device, family, index, p_queue);
    dev.queue_families.lock().unwrap().insert(*p_queue, family);
}

pub unsafe extern "system" fn get_device_queue2(
    device: vk::Device,
    p_info: *const vk::DeviceQueueInfo2,
    p_queue: *mut vk::Queue,
) {
    let Some(dev) = state::device(device) else { return };
    (dev.fns.fp_v1_1().get_device_queue2)(device, p_info, p_queue);
    if !(*p_queue).is_null() {
        dev.queue_families.lock().unwrap().insert(*p_queue, (*p_info).queue_family_index);
    }
}

// --------------------------------------------------------------- swapchain

/// The other (sRGB/UNORM) spelling of a 32-bit format, so the application's
/// own image views stay legal on a promoted swapchain.
fn format_sibling(format: vk::Format) -> Option<vk::Format> {
    Some(match format {
        vk::Format::B8G8R8A8_UNORM => vk::Format::B8G8R8A8_SRGB,
        vk::Format::B8G8R8A8_SRGB => vk::Format::B8G8R8A8_UNORM,
        vk::Format::R8G8B8A8_UNORM => vk::Format::R8G8B8A8_SRGB,
        vk::Format::R8G8B8A8_SRGB => vk::Format::R8G8B8A8_UNORM,
        vk::Format::A8B8G8R8_UNORM_PACK32 => vk::Format::A8B8G8R8_SRGB_PACK32,
        vk::Format::A8B8G8R8_SRGB_PACK32 => vk::Format::A8B8G8R8_UNORM_PACK32,
        _ => return None,
    })
}

/// Formats an application renders into that can share an image with the
/// HDR10 view: the same format (only the color space changes, no copy
/// needed), or another 32-bit format of the same Vulkan compatibility class.
fn is_promotable(format: vk::Format) -> bool {
    format == HDR10_FORMAT || format_sibling(format).is_some()
}

const HDR10_FORMAT: vk::Format = vk::Format::A2B10G10R10_UNORM_PACK32;

unsafe fn surface_formats(dev: &DeviceData, surface: vk::SurfaceKHR) -> Vec<vk::SurfaceFormatKHR> {
    let f = dev.instance.surface_fn.get_physical_device_surface_formats_khr;
    let mut count = 0;
    if f(dev.physical_device, surface, &mut count, std::ptr::null_mut()) != vk::Result::SUCCESS {
        return Vec::new();
    }
    let mut formats = vec![vk::SurfaceFormatKHR::default(); count as usize];
    if f(dev.physical_device, surface, &mut count, formats.as_mut_ptr()) != vk::Result::SUCCESS {
        return Vec::new();
    }
    formats.truncate(count as usize);
    formats
}

/// Whether to hand the application an HDR10 swapchain it never asked for.
///
/// The application keeps rendering 8-bit SDR through a view in its own
/// format; the filter chain reads those pixels and writes PQ through the
/// HDR10 view of the same images. This is what lets an HDR preset produce
/// real HDR out of an SDR game, the way RetroArch does.
unsafe fn promote_to_hdr10(dev: &DeviceData, ci: &vk::SwapchainCreateInfoKHR) -> bool {
    let cfg = config::get();
    if cfg.hdr_output == HdrOutput::Off || !dev.mutable_format || !is_promotable(ci.image_format) {
        return false;
    }
    if cfg.hdr_output == HdrOutput::Auto {
        // Only when the preset actually writes HDR.
        let preset_is_hdr = crate::control::control().preset_color_space.is_some_and(|cs| cs.is_hdr());
        if !preset_is_hdr {
            return false;
        }
    }
    if ci.image_color_space == vk::ColorSpaceKHR::HDR10_ST2084_EXT {
        return false; // already HDR10
    }
    let formats = surface_formats(dev, ci.surface);
    for f in &formats {
        log_debug!("surface offers {:?} / {:?}", f.format, f.color_space);
    }
    let supported = formats
        .iter()
        .any(|f| f.format == HDR10_FORMAT && f.color_space == vk::ColorSpaceKHR::HDR10_ST2084_EXT);
    if !supported {
        log_warn!("the surface does not offer HDR10, keeping the SDR swapchain");
    }
    supported
}

/// Decides whether a swapchain can be processed. Returns the format
/// librashader renders to and whether the swapchain must be made mutable.
unsafe fn plan_swapchain(dev: &DeviceData, ci: &vk::SwapchainCreateInfoKHR) -> Option<(vk::Format, bool)> {
    if matches!(
        ci.present_mode,
        vk::PresentModeKHR::SHARED_DEMAND_REFRESH | vk::PresentModeKHR::SHARED_CONTINUOUS_REFRESH
    ) || ci.image_array_layers != 1
    {
        log_warn!("unsupported swapchain (shared present mode or layered), passing through");
        return None;
    }

    let mut caps = vk::SurfaceCapabilitiesKHR::default();
    let r = (dev.instance.surface_fn.get_physical_device_surface_capabilities_khr)(
        dev.physical_device,
        ci.surface,
        &mut caps,
    );
    let usage = vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC;
    if r != vk::Result::SUCCESS || !caps.supported_usage_flags.contains(usage) {
        log_warn!("surface does not support color attachment + transfer src usage, passing through");
        return None;
    }

    let props = dev
        .instance
        .fns
        .get_physical_device_format_properties(dev.physical_device, ci.image_format);
    let features = vk::FormatFeatureFlags::BLIT_SRC
        | vk::FormatFeatureFlags::BLIT_DST
        | vk::FormatFeatureFlags::SAMPLED_IMAGE
        | vk::FormatFeatureFlags::COLOR_ATTACHMENT;
    if !props.optimal_tiling_features.contains(features) {
        log_warn!("format {:?} lacks blit/sample/attachment support, passing through", ci.image_format);
        return None;
    }

    // sRGB swapchain: render through a UNORM view when possible, unless the
    // application already constrains view formats with its own list.
    let app_has_format_list = {
        let mut p = ci.p_next as *const vk::BaseInStructure;
        let mut found = false;
        while !p.is_null() {
            found |= (*p).s_type == vk::StructureType::IMAGE_FORMAT_LIST_CREATE_INFO;
            p = (*p).p_next;
        }
        found
    };
    match srgb_to_unorm(ci.image_format) {
        Some(unorm) if dev.mutable_format && !app_has_format_list => Some((unorm, true)),
        Some(_) => {
            log_warn!("sRGB swapchain without mutable format support: output gamma may be off");
            Some((ci.image_format, false))
        }
        None => Some((ci.image_format, false)),
    }
}

pub unsafe extern "system" fn create_swapchain(
    device: vk::Device,
    p_create_info: *const vk::SwapchainCreateInfoKHR,
    p_allocator: *const vk::AllocationCallbacks,
    p_swapchain: *mut vk::SwapchainKHR,
) -> vk::Result {
    let Some(dev) = state::device(device) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let next = dev.swapchain_fn.create_swapchain_khr;
    let ci = &*p_create_info;
    if !dev.active {
        return next(device, p_create_info, p_allocator, p_swapchain);
    }

    // The preset is loaded first: whether it writes HDR decides whether the
    // swapchain is promoted to HDR10 below.
    let mut guard = dev.runtime.lock().unwrap();
    if guard.is_none() {
        match Runtime::new(&dev) {
            Ok(rt) => *guard = Some(rt),
            Err(e) => {
                log_error!("cannot create layer runtime: {e}");
                drop(guard);
                return next(device, p_create_info, p_allocator, p_swapchain);
            }
        }
    }
    guard.as_mut().unwrap().ensure_chain(&dev);

    let Some((mut output_format, mutable)) = plan_swapchain(&dev, ci) else {
        drop(guard);
        return next(device, p_create_info, p_allocator, p_swapchain);
    };

    let mut modified = *ci;
    modified.image_usage |= vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC;

    // Note: the layer does NOT ask for extra swapchain images for subframes.
    // They are presented one at a time, so a free image is enough, and
    // raising the count breaks applications that size their own swapchain
    // arrays statically (Qt's QVulkanWindow crashes in the driver).

    // Application format kept for the raw-bit copy of what the game renders.
    let app_format = ci.image_format;
    let promoted = promote_to_hdr10(&dev, ci);
    // Same format on both sides: only the color space changes, the pixels the
    // application writes are read back directly.
    let needs_staging = promoted && app_format != HDR10_FORMAT;
    if promoted {
        modified.image_format = HDR10_FORMAT;
        modified.image_color_space = vk::ColorSpaceKHR::HDR10_ST2084_EXT;
        output_format = HDR10_FORMAT;
    }

    let view_formats: Vec<vk::Format> = if needs_staging {
        // The application keeps rendering through its own format.
        [Some(HDR10_FORMAT), Some(app_format), format_sibling(app_format)].into_iter().flatten().collect()
    } else {
        vec![ci.image_format, output_format]
    };
    let mut format_list = vk::ImageFormatListCreateInfo::default().view_formats(&view_formats);
    if needs_staging || mutable {
        modified.flags |= vk::SwapchainCreateFlagsKHR::MUTABLE_FORMAT;
        format_list.p_next = modified.p_next;
        modified.p_next = &format_list as *const _ as *const c_void;
    }

    let result = next(device, &modified, p_allocator, p_swapchain);
    if result != vk::Result::SUCCESS {
        if promoted {
            log_error!("HDR10 swapchain refused ({result}), retrying as the application asked");
            drop(guard);
            return next(device, p_create_info, p_allocator, p_swapchain);
        }
        drop(guard);
        return result;
    }
    let swapchain = *p_swapchain;

    let images = {
        let get = dev.swapchain_fn.get_swapchain_images_khr;
        let mut count = 0;
        let _ = get(device, swapchain, &mut count, std::ptr::null_mut());
        let mut images = vec![vk::Image::null(); count as usize];
        let r = get(device, swapchain, &mut count, images.as_mut_ptr());
        images.truncate(count as usize);
        (r == vk::Result::SUCCESS).then_some(images)
    };

    let plan = SwapchainPlan {
        extent: ci.image_extent,
        format: modified.image_format,
        app_format,
        output_format,
        color_space: modified.image_color_space,
        promoted: needs_staging,
        hdr_promoted: promoted,
    };
    let rt = guard.as_mut().unwrap();
    // Tracked even if the preset failed to load: another one can be loaded
    // live from vkslang-ui.
    match images.map(|imgs| SwapchainState::new(&dev, &plan, imgs)) {
        Some(Ok(state)) => {
            log_info!(
                "processing swapchain {}x{} {:?}{}",
                ci.image_extent.width,
                ci.image_extent.height,
                modified.image_format,
                if promoted { " (promoted to HDR10)" } else { "" }
            );
            rt.track_swapchain(swapchain, state);
        }
        Some(Err(e)) => log_error!("cannot create swapchain resources: {e}"),
        None => log_error!("vkGetSwapchainImagesKHR failed"),
    }
    result
}

pub unsafe extern "system" fn destroy_swapchain(
    device: vk::Device,
    swapchain: vk::SwapchainKHR,
    p_allocator: *const vk::AllocationCallbacks,
) {
    let Some(dev) = state::device(device) else { return };
    if let Some(rt) = dev.runtime.lock().unwrap().as_mut() {
        rt.forget_swapchain(&dev, swapchain);
    }
    (dev.swapchain_fn.destroy_swapchain_khr)(device, swapchain, p_allocator);
}

pub unsafe extern "system" fn queue_present(queue: vk::Queue, p_present_info: *const vk::PresentInfoKHR) -> vk::Result {
    // A VkQueue shares its dispatch key with its VkDevice.
    let Some(dev) = state::device(queue) else {
        return vk::Result::ERROR_DEVICE_LOST;
    };
    let next = dev.swapchain_fn.queue_present_khr;
    if !dev.active {
        return next(queue, p_present_info);
    }
    let pi = &*p_present_info;

    // Submit on the presenting queue when it belongs to our command pool's
    // family: the application is already synchronizing that queue for us.
    let same_family = dev.queue_families.lock().unwrap().get(&queue) == Some(&dev.queue_family);
    let submit_queue = if same_family { queue } else { dev.queue };

    let mut guard = dev.runtime.lock().unwrap();
    let Some(rt) = guard.as_mut() else {
        drop(guard);
        return next(queue, p_present_info);
    };
    // Apply what vkslang-ui changed since the last frame.
    rt.sync(&dev);
    if !rt.is_rendering() {
        drop(guard);
        return next(queue, p_present_info);
    }

    let swapchains = slice(pi.p_swapchains, pi.swapchain_count);
    let indices = slice(pi.p_image_indices, pi.swapchain_count);
    let app_waits = slice(pi.p_wait_semaphores, pi.wait_semaphore_count);
    let mut waits = Vec::with_capacity(swapchains.len());
    let mut processed = Vec::with_capacity(swapchains.len());
    for (&swapchain, &index) in swapchains.iter().zip(indices) {
        // The application's semaphores are consumed by our first submission.
        let wait: &[vk::Semaphore] = if waits.is_empty() { app_waits } else { &[] };
        match rt.render(&dev, submit_queue, swapchain, index, wait, None) {
            Ok(Some(sem)) => {
                waits.push(sem);
                processed.push(swapchain);
            }
            Ok(None) => {}
            Err(e) => {
                log_error!("present hook failed: {e}");
                if waits.is_empty() {
                    drop(guard);
                    return next(queue, p_present_info);
                }
                return e;
            }
        }
    }
    if waits.is_empty() {
        drop(guard);
        return next(queue, p_present_info);
    }
    let mut info = *pi;
    info.wait_semaphore_count = waits.len() as u32;
    info.p_wait_semaphores = waits.as_ptr();
    let result = next(queue, &info);

    // Extra presentations of the same frame, so interlacing presets alternate
    // fields faster than the application draws (or for black frame insertion).
    let (subframes, black) = {
        let ctl = crate::control::control();
        (ctl.subframes, ctl.subframe_black)
    };
    if subframes > 1 && matches!(result, vk::Result::SUCCESS | vk::Result::SUBOPTIMAL_KHR) {
        for swapchain in processed {
            rt.present_subframes(&dev, submit_queue, swapchain, subframes, black);
        }
    }
    drop(guard);
    result
}
