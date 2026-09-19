//! Per-device runtime: librashader filter chain, frame ring and swapchain
//! resources, plus the command recording done at present time.
//!
//! Frame recipe (one submission per presented swapchain image):
//!
//! ```text
//!  swapchain image  PRESENT_SRC ──► TRANSFER_SRC ──blit/copy──► source image (source_res)
//!                                                              TRANSFER_DST ──► SHADER_READ_ONLY
//!  swapchain image  TRANSFER_SRC ──► COLOR_ATTACHMENT ◄── librashader passes (Original = source)
//!  swapchain image  COLOR_ATTACHMENT ──► PRESENT_SRC
//! ```
//!
//! Live control (vkslang-ui): [`Runtime::sync`] runs at the start of every
//! present and applies what changed in [`crate::control`]: parameters are
//! pushed to the chain, source settings rebuild the source images, and a new
//! preset is compiled on a background thread then swapped in.

use crate::config::Source;
use crate::control::{control, Control};
use crate::state::DeviceData;
use crate::{log_debug, log_error, log_info, log_warn};
use ash::vk;
use librashader::presets::{get_parameter_meta, PresetColorSpace, ShaderFeatures, ShaderPreset};
use librashader::runtime::vk::{FilterChain, FilterChainOptions, FrameOptions, VulkanImage};
use librashader::runtime::{ColorSpace, FilterChainParameters, Size, Viewport};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;
use vkslang_ipc::{HdrSettings, Output, Param};

/// Number of frames the layer keeps in flight (command buffer + fence each).
const RING: usize = 3;

pub fn srgb_to_unorm(format: vk::Format) -> Option<vk::Format> {
    Some(match format {
        vk::Format::B8G8R8A8_SRGB => vk::Format::B8G8R8A8_UNORM,
        vk::Format::R8G8B8A8_SRGB => vk::Format::R8G8B8A8_UNORM,
        vk::Format::A8B8G8R8_SRGB_PACK32 => vk::Format::A8B8G8R8_UNORM_PACK32,
        _ => return None,
    })
}

fn to_ipc(cs: ColorSpace) -> vkslang_ipc::ColorSpace {
    match cs {
        ColorSpace::Sdr => vkslang_ipc::ColorSpace::Sdr,
        ColorSpace::Hdr10 => vkslang_ipc::ColorSpace::Hdr10,
        ColorSpace::ScRgb => vkslang_ipc::ColorSpace::ScRgb,
        ColorSpace::PqScRgb => vkslang_ipc::ColorSpace::PqScRgb,
    }
}

fn warn_mismatch(preset: ColorSpace, output: ColorSpace) {
    if let Some(why) = vkslang_ipc::color_space_mismatch(to_ipc(preset), to_ipc(output)) {
        log_warn!("{why}");
    }
}

fn color_space(cs: vk::ColorSpaceKHR) -> ColorSpace {
    match cs {
        vk::ColorSpaceKHR::HDR10_ST2084_EXT => ColorSpace::Hdr10,
        vk::ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT => ColorSpace::ScRgb,
        _ => ColorSpace::Sdr,
    }
}

// ------------------------------------------------------------ preset loading

/// `#pragma parameter` names in declaration order, following `#include`s,
/// so the UI lists parameters the way RetroArch does.
fn pragma_order(path: &Path, order: &mut Vec<String>, seen: &mut HashSet<String>, depth: u32) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    for line in text.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("#include") {
            if let (Some(a), Some(b)) = (rest.find('"'), rest.rfind('"')) {
                if a < b && depth < 16 {
                    let inc = path.parent().unwrap_or(Path::new(".")).join(&rest[a + 1..b]);
                    pragma_order(&inc, order, seen, depth + 1);
                }
            }
        } else if let Some(rest) = line.strip_prefix("#pragma parameter") {
            if let Some(name) = rest.split_whitespace().next() {
                if seen.insert(name.to_string()) {
                    order.push(name.to_string());
                }
            }
        }
    }
}

fn preset_params(preset: &ShaderPreset) -> Result<Vec<Param>, String> {
    let mut meta: HashMap<String, Param> = get_parameter_meta(preset)
        .map_err(|e| e.to_string())?
        .map(|p| {
            let name = p.id.to_string();
            let param = Param {
                name: name.clone(),
                description: p.description,
                initial: p.initial,
                minimum: p.minimum,
                maximum: p.maximum,
                step: p.step,
                value: p.initial,
            };
            (name, param)
        })
        .collect();
    let (mut order, mut seen) = (Vec::new(), HashSet::new());
    for pass in &preset.passes {
        pragma_order(&pass.path, &mut order, &mut seen, 0);
    }
    let mut params: Vec<Param> = order.iter().filter_map(|n| meta.remove(n)).collect();
    let mut rest: Vec<Param> = meta.into_values().collect();
    rest.sort_by(|a, b| a.name.cmp(&b.name));
    params.extend(rest);
    Ok(params)
}

/// A compiled preset waiting to be installed. `cmd` (allocated from its own
/// `pool`, so loading can run off-thread) holds the GPU uploads.
pub struct Loaded {
    path: PathBuf,
    chain: FilterChain,
    params: Vec<Param>,
    /// Color space the final pass writes.
    color_space: ColorSpace,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
}

unsafe fn allocate_cmd(dev: &DeviceData, pool: vk::CommandPool) -> Result<vk::CommandBuffer, vk::Result> {
    let cmd = dev.fns.allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1),
    )?[0];
    // Our command buffers never go through the loader trampoline, so the
    // dispatch pointer has to be patched for layers/ICDs below us.
    let r = (dev.set_loader_data)(dev.handle, vk::Handle::as_raw(cmd) as usize as *mut c_void);
    if r != vk::Result::SUCCESS {
        dev.fns.free_command_buffers(pool, &[cmd]);
        return Err(r);
    }
    Ok(cmd)
}

/// Parses and compiles a preset. Only thread-safe `vkCreate*` calls and a
/// private command pool are used, so this may run on any thread.
unsafe fn load_chain(dev: &DeviceData, path: &Path) -> Result<Loaded, String> {
    let started = Instant::now();
    let path = &crate::config::resolve_path(path);
    let preset = ShaderPreset::try_parse(path, ShaderFeatures::NONE).map_err(|e| e.to_string())?;
    let params = preset_params(&preset)?;
    let color_space = preset.color_space().unwrap_or_else(|e| {
        log_warn!("cannot determine the preset's output color space: {e}");
        ColorSpace::Sdr
    });

    let d = &dev.fns;
    let pool = d
        .create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .flags(vk::CommandPoolCreateFlags::TRANSIENT)
                .queue_family_index(dev.queue_family),
            None,
        )
        .map_err(|e| e.to_string())?;
    let fail = |e: String| {
        d.destroy_command_pool(pool, None);
        e
    };
    let cmd = allocate_cmd(dev, pool).map_err(|e| fail(e.to_string()))?;
    d.begin_command_buffer(
        cmd,
        &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
    )
    .map_err(|e| fail(e.to_string()))?;
    let options = FilterChainOptions {
        // One extra frame of slack before librashader recycles per-frame
        // image views/framebuffers.
        frames_in_flight: RING as u32 + 1,
        force_no_mipmaps: false,
        // Dynamic rendering is only usable if the *application* enabled the
        // feature; render passes always work.
        use_dynamic_rendering: false,
        disable_cache: false,
    };
    let vulkan = (dev.physical_device, dev.instance.fns.clone(), dev.fns.clone(), dev.queue);
    let chain = FilterChain::load_from_preset_deferred(preset, vulkan, cmd, Some(&options))
        .map_err(|e| fail(e.to_string()))?;
    d.end_command_buffer(cmd).map_err(|e| fail(e.to_string()))?;
    log_info!("compiled {} in {:.2?}", path.display(), started.elapsed());
    Ok(Loaded { path: path.to_path_buf(), chain, params, color_space, pool, cmd })
}

// ---------------------------------------------------------------- swapchain

struct SourceImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    extent: vk::Extent2D,
    /// View format librashader uses to sample it (always non-sRGB, like a
    /// RetroArch core framebuffer).
    view_format: vk::Format,
    /// Region of the swapchain image holding the game picture.
    rect: vk::Rect2D,
    filter: vk::Filter,
}

impl SourceImage {
    unsafe fn new(
        dev: &DeviceData,
        format: vk::Format,
        swapchain_extent: vk::Extent2D,
        source: &Source,
    ) -> Result<SourceImage, vk::Result> {
        let d = &dev.fns;
        let rect = source.picture_rect(swapchain_extent);
        let extent = source.res.unwrap_or(rect.extent);
        let view_format = srgb_to_unorm(format).unwrap_or(format);

        let mut flags = vk::ImageCreateFlags::empty();
        if view_format != format {
            // Blit sRGB->sRGB keeps the encoded bytes; the chain then samples
            // them through a UNORM view, like RetroArch does.
            flags |= vk::ImageCreateFlags::MUTABLE_FORMAT;
        }
        let image = d.create_image(
            &vk::ImageCreateInfo::default()
                .flags(flags)
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D { width: extent.width, height: extent.height, depth: 1 })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                // TRANSFER_SRC: librashader copies Original into its history.
                .usage(
                    vk::ImageUsageFlags::TRANSFER_DST
                        | vk::ImageUsageFlags::TRANSFER_SRC
                        | vk::ImageUsageFlags::SAMPLED,
                )
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            None,
        )?;

        let reqs = d.get_image_memory_requirements(image);
        let props = dev.instance.fns.get_physical_device_memory_properties(dev.physical_device);
        let Some(type_index) = (0..props.memory_type_count).find(|&i| {
            reqs.memory_type_bits & (1 << i) != 0
                && props.memory_types[i as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        }) else {
            d.destroy_image(image, None);
            return Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
        };
        let memory = match d.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(reqs.size)
                .memory_type_index(type_index),
            None,
        ) {
            Ok(m) => m,
            Err(e) => {
                d.destroy_image(image, None);
                return Err(e);
            }
        };
        let source = SourceImage { image, memory, extent, view_format, rect, filter: source.filter };
        if let Err(e) = d.bind_image_memory(image, memory, 0) {
            source.destroy(dev);
            return Err(e);
        }
        log_debug!(
            "source {}x{} from picture {:?} of {}x{}",
            extent.width, extent.height, rect, swapchain_extent.width, swapchain_extent.height
        );
        Ok(source)
    }

    unsafe fn destroy(self, dev: &DeviceData) {
        dev.fns.destroy_image(self.image, None);
        dev.fns.free_memory(self.memory, None);
    }
}

pub struct SwapchainState {
    images: Vec<vk::Image>,
    /// Signalled by our submission, waited on by the real present. Indexed by
    /// image index: reacquiring an image guarantees its previous present
    /// consumed the semaphore.
    semaphores: Vec<vk::Semaphore>,
    format: vk::Format,
    extent: vk::Extent2D,
    /// Format librashader renders to (UNORM view of an sRGB swapchain when the
    /// device allows mutable swapchain formats).
    output_format: vk::Format,
    color_space: ColorSpace,
    /// Low-resolution copy fed to the chain as `Original`. `None` if it could
    /// not be created with the current settings (frames pass through).
    source: Option<SourceImage>,
}

impl SwapchainState {
    /// `images` are the swapchain images returned by the next layer.
    pub unsafe fn new(
        dev: &DeviceData,
        info: &vk::SwapchainCreateInfoKHR,
        images: Vec<vk::Image>,
        output_format: vk::Format,
    ) -> Result<SwapchainState, vk::Result> {
        let mut state = SwapchainState {
            semaphores: Vec::with_capacity(images.len()),
            images,
            format: info.image_format,
            extent: info.image_extent,
            output_format,
            color_space: color_space(info.image_color_space),
            source: None,
        };
        for _ in 0..state.images.len() {
            match dev.fns.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) {
                Ok(s) => state.semaphores.push(s),
                Err(e) => {
                    state.destroy(dev);
                    return Err(e);
                }
            }
        }
        let source = control().source.clone();
        state.rebuild_source(dev, &source);
        Ok(state)
    }

    /// Caller guarantees the GPU no longer uses the previous source image.
    unsafe fn rebuild_source(&mut self, dev: &DeviceData, source: &Source) {
        if let Some(old) = self.source.take() {
            old.destroy(dev);
        }
        match SourceImage::new(dev, self.format, self.extent, source) {
            Ok(s) => self.source = Some(s),
            Err(e) => log_error!("cannot create source image: {e}"),
        }
    }

    unsafe fn destroy(mut self, dev: &DeviceData) {
        for s in self.semaphores.drain(..) {
            dev.fns.destroy_semaphore(s, None);
        }
        if let Some(source) = self.source.take() {
            source.destroy(dev);
        }
    }
}

// ------------------------------------------------------------------ runtime

struct FrameSlot {
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    /// Preset upload submitted with this slot; its pool is destroyed once the
    /// fence signals.
    init: Option<(vk::CommandPool, vk::CommandBuffer)>,
}

struct ActiveChain {
    chain: FilterChain,
    color_space: ColorSpace,
    /// Metadata; `initial` is the preset's own value.
    params: Vec<Param>,
}

pub struct Runtime {
    pool: vk::CommandPool,
    slots: Vec<FrameSlot>,
    next_slot: usize,
    chain: Option<ActiveChain>,
    /// Upload of the installed chain, submitted with the next frame.
    pending_init: Option<(vk::CommandPool, vk::CommandBuffer)>,
    loader: Option<JoinHandle<Result<Loaded, String>>>,
    /// Set after a frame error: presents pass through until another preset
    /// is installed.
    failed: bool,
    enabled: bool,
    hdr: HdrSettings,
    applied_preset_gen: Option<u64>,
    applied_params_gen: Option<u64>,
    applied_source_gen: u64,
    swapchains: HashMap<vk::SwapchainKHR, SwapchainState>,
    frame_count: usize,
    last_frame: Option<Instant>,
}

impl Runtime {
    pub unsafe fn new(dev: &DeviceData) -> Result<Runtime, vk::Result> {
        let d = &dev.fns;
        let pool = d.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                .queue_family_index(dev.queue_family),
            None,
        )?;
        // One lock at a time: guards created inside the struct literal below
        // would all live until the end of the statement (self-deadlock).
        let (hdr, source_gen) = {
            let ctl = control();
            (ctl.hdr, ctl.source_gen)
        };
        let mut rt = Runtime {
            pool,
            slots: Vec::with_capacity(RING),
            next_slot: 0,
            chain: None,
            pending_init: None,
            loader: None,
            failed: false,
            enabled: true,
            hdr,
            applied_preset_gen: None,
            applied_params_gen: None,
            applied_source_gen: source_gen,
            swapchains: HashMap::new(),
            frame_count: 0,
            last_frame: None,
        };
        for _ in 0..RING {
            let slot = allocate_cmd(dev, pool).and_then(|cmd| {
                let fence = d.create_fence(
                    &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                    None,
                )?;
                Ok(FrameSlot { cmd, fence, init: None })
            });
            match slot {
                Ok(slot) => rt.slots.push(slot),
                Err(e) => {
                    rt.destroy(dev);
                    return Err(e);
                }
            }
        }
        Ok(rt)
    }

    /// First preset load, done synchronously at swapchain creation so the
    /// very first frames are already processed. Later loads are async.
    pub unsafe fn ensure_chain(&mut self, dev: &DeviceData) {
        if self.chain.is_some() || self.loader.is_some() || self.applied_preset_gen.is_some() {
            return;
        }
        let (path, gen) = {
            let ctl = control();
            (ctl.preset.clone(), ctl.preset_gen)
        };
        self.applied_preset_gen = Some(gen);
        let Some(path) = path else { return };
        control().loading = true;
        let result = load_chain(dev, &path);
        self.finish_load(dev, result);
    }

    unsafe fn finish_load(&mut self, dev: &DeviceData, result: Result<Loaded, String>) {
        let mut ctl = control();
        ctl.loading = false;
        match result {
            Ok(loaded) => self.install(dev, loaded, &mut ctl),
            Err(e) => {
                log_error!("failed to load preset: {e}");
                ctl.error = Some(e);
            }
        }
    }

    unsafe fn install(&mut self, dev: &DeviceData, loaded: Loaded, ctl: &mut Control) {
        // The previous chain may still be referenced by frames in flight.
        self.wait_all(dev);
        if let Some((pool, _)) = self.pending_init.take() {
            // Never submitted (layer was disabled); safe to drop right away.
            dev.fns.destroy_command_pool(pool, None);
        }
        for name in ctl.overrides.keys() {
            if !loaded.params.iter().any(|p| &p.name == name) {
                log_warn!("preset has no parameter '{name}'");
            }
        }
        ctl.params = loaded
            .params
            .iter()
            .map(|p| Param { value: ctl.param_value(&p.name, p.initial), ..p.clone() })
            .collect();
        ctl.running_preset = Some(loaded.path.clone());
        ctl.preset_color_space = Some(to_ipc(loaded.color_space));
        ctl.error = None;
        for state in self.swapchains.values() {
            warn_mismatch(loaded.color_space, state.color_space);
        }
        self.chain = Some(ActiveChain {
            chain: loaded.chain,
            color_space: loaded.color_space,
            params: loaded.params,
        });
        self.pending_init = Some((loaded.pool, loaded.cmd));
        self.failed = false;
        self.applied_params_gen = None;
        log_info!("running {}", loaded.path.display());
    }

    /// Applies pending changes from the control state. Called at every
    /// present, before rendering, under the runtime lock.
    pub unsafe fn sync(&mut self, dev: &Arc<DeviceData>) {
        // Background load finished?
        if self.loader.as_ref().is_some_and(|h| h.is_finished()) {
            let result = self.loader.take().unwrap().join().unwrap_or_else(|_| Err("loader panicked".into()));
            self.finish_load(dev, result);
        }

        let mut ctl = control();
        self.enabled = ctl.enabled;
        self.hdr = ctl.hdr;

        // New preset requested (one load at a time; a newer request made
        // meanwhile starts as soon as the current one is installed).
        if self.loader.is_none() && self.applied_preset_gen != Some(ctl.preset_gen) {
            self.applied_preset_gen = Some(ctl.preset_gen);
            if let Some(path) = ctl.preset.clone() {
                ctl.loading = true;
                let dev = Arc::clone(dev);
                let spawned = std::thread::Builder::new()
                    .name("vkslang-load".into())
                    .spawn(move || unsafe { load_chain(&dev, &path) });
                match spawned {
                    Ok(handle) => self.loader = Some(handle),
                    Err(e) => {
                        ctl.loading = false;
                        ctl.error = Some(format!("cannot spawn loader: {e}"));
                    }
                }
            }
        }

        // Parameters: cheap uniform updates, no GPU sync needed.
        if let Some(active) = &self.chain {
            if self.applied_params_gen != Some(ctl.params_gen) {
                self.applied_params_gen = Some(ctl.params_gen);
                let runtime = active.chain.parameters();
                for p in &active.params {
                    let value = ctl.param_value(&p.name, p.initial);
                    if runtime.parameter_value(&p.name) != Some(value) {
                        runtime.set_parameter_value(&p.name, value);
                    }
                }
            }
        }

        // Source settings: rebuild the source images.
        if self.applied_source_gen != ctl.source_gen {
            self.applied_source_gen = ctl.source_gen;
            let source = ctl.source.clone();
            drop(ctl);
            self.wait_all(dev);
            for state in self.swapchains.values_mut() {
                state.rebuild_source(dev, &source);
            }
        }
    }

    pub fn is_rendering(&self) -> bool {
        self.chain.is_some() && self.enabled && !self.failed
    }

    fn publish_outputs(&self) {
        control().outputs = self
            .swapchains
            .values()
            .map(|s| Output {
                size: [s.extent.width, s.extent.height],
                format: format!("{:?}", s.format),
                color_space: to_ipc(s.color_space),
            })
            .collect();
    }

    pub fn track_swapchain(&mut self, swapchain: vk::SwapchainKHR, state: SwapchainState) {
        if let Some(active) = &self.chain {
            warn_mismatch(active.color_space, state.color_space);
        }
        self.swapchains.insert(swapchain, state);
        self.publish_outputs();
    }

    unsafe fn wait_all(&mut self, dev: &DeviceData) {
        let fences: Vec<_> = self.slots.iter().map(|s| s.fence).collect();
        if !fences.is_empty() {
            let _ = dev.fns.wait_for_fences(&fences, true, u64::MAX);
        }
        for slot in &mut self.slots {
            if let Some((pool, _)) = slot.init.take() {
                dev.fns.destroy_command_pool(pool, None);
            }
        }
    }

    pub unsafe fn forget_swapchain(&mut self, dev: &DeviceData, swapchain: vk::SwapchainKHR) {
        if let Some(state) = self.swapchains.remove(&swapchain) {
            self.wait_all(dev);
            state.destroy(dev);
            self.publish_outputs();
        }
    }

    pub unsafe fn destroy(mut self, dev: &DeviceData) {
        // A preset may still be compiling against this device.
        if let Some(Ok(loaded)) = self.loader.take().and_then(|h| h.join().ok()) {
            drop(loaded.chain);
            dev.fns.destroy_command_pool(loaded.pool, None);
        }
        let _ = dev.fns.device_wait_idle();
        for (_, state) in self.swapchains.drain() {
            state.destroy(dev);
        }
        // Drops pipelines, images and the gpu-allocator while the device lives.
        self.chain = None;
        if let Some((pool, _)) = self.pending_init.take() {
            dev.fns.destroy_command_pool(pool, None);
        }
        for slot in &mut self.slots {
            if let Some((pool, _)) = slot.init.take() {
                dev.fns.destroy_command_pool(pool, None);
            }
            dev.fns.destroy_fence(slot.fence, None);
        }
        // Destroying the pool frees every command buffer allocated from it.
        dev.fns.destroy_command_pool(self.pool, None);
        control().outputs.clear();
    }

    /// Records and submits the filter chain for one presented image. Returns
    /// the semaphore the present must wait on, or `None` to leave this
    /// swapchain untouched.
    pub unsafe fn render(
        &mut self,
        dev: &DeviceData,
        queue: vk::Queue,
        swapchain: vk::SwapchainKHR,
        image_index: u32,
        wait_semaphores: &[vk::Semaphore],
    ) -> Result<Option<vk::Semaphore>, vk::Result> {
        if !self.is_rendering() {
            return Ok(None);
        }
        let Runtime {
            slots, next_slot, chain, failed, hdr, pending_init, swapchains, frame_count, last_frame, ..
        } = self;
        let (Some(state), Some(active)) = (swapchains.get(&swapchain), chain.as_mut()) else {
            return Ok(None);
        };
        let (Some(&image), Some(source)) = (state.images.get(image_index as usize), state.source.as_ref()) else {
            return Ok(None);
        };
        let d = &dev.fns;

        let slot = &mut slots[*next_slot];
        *next_slot = (*next_slot + 1) % RING;
        d.wait_for_fences(&[slot.fence], true, u64::MAX)?;
        if let Some((pool, _)) = slot.init.take() {
            d.destroy_command_pool(pool, None);
        }
        let cmd = slot.cmd;
        d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
        d.begin_command_buffer(
            cmd,
            &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;

        let color = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let barrier = |img, old, new, src_access, dst_access| {
            vk::ImageMemoryBarrier::default()
                .image(img)
                .old_layout(old)
                .new_layout(new)
                .src_access_mask(src_access)
                .dst_access_mask(dst_access)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .subresource_range(color)
        };

        // 1. swapchain -> TRANSFER_SRC, source -> TRANSFER_DST (previous
        //    contents discarded; the chain keeps its own history copies).
        d.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[
                barrier(
                    image,
                    vk::ImageLayout::PRESENT_SRC_KHR,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::AccessFlags::MEMORY_WRITE,
                    vk::AccessFlags::TRANSFER_READ,
                ),
                barrier(
                    source.image,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::TRANSFER_WRITE,
                ),
            ],
        );

        // 2. Downsample the picture region to the logical source resolution.
        let r = source.rect;
        let layers = vk::ImageSubresourceLayers::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .layer_count(1);
        if r.extent == source.extent {
            d.cmd_copy_image(
                cmd,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                source.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[vk::ImageCopy::default()
                    .src_subresource(layers)
                    .src_offset(vk::Offset3D { x: r.offset.x, y: r.offset.y, z: 0 })
                    .dst_subresource(layers)
                    .extent(vk::Extent3D { width: r.extent.width, height: r.extent.height, depth: 1 })],
            );
        } else {
            let s = source.extent;
            d.cmd_blit_image(
                cmd,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                source.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[vk::ImageBlit::default()
                    .src_subresource(layers)
                    .src_offsets([
                        vk::Offset3D { x: r.offset.x, y: r.offset.y, z: 0 },
                        vk::Offset3D {
                            x: r.offset.x + r.extent.width as i32,
                            y: r.offset.y + r.extent.height as i32,
                            z: 1,
                        },
                    ])
                    .dst_subresource(layers)
                    .dst_offsets([
                        vk::Offset3D::default(),
                        vk::Offset3D { x: s.width as i32, y: s.height as i32, z: 1 },
                    ])],
                source.filter,
            );
        }

        // 3. source -> SHADER_READ_ONLY (librashader input contract),
        //    swapchain -> COLOR_ATTACHMENT (librashader output contract).
        d.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::FRAGMENT_SHADER
                | vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
                | vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[
                barrier(
                    source.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::TRANSFER_WRITE,
                    vk::AccessFlags::SHADER_READ | vk::AccessFlags::TRANSFER_READ,
                ),
                barrier(
                    image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::COLOR_ATTACHMENT_READ | vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                ),
            ],
        );

        // 4. Run the preset. The viewport is the picture region, so pillar/
        //    letterbox bars are cleared to black by librashader's final pass.
        let now = Instant::now();
        let options = FrameOptions {
            frametime_delta: last_frame.map_or(0, |t| now.duration_since(t).as_millis() as u32),
            // Binds HDRMode / BrightnessNits / ExpandGamut for HDR presets.
            color_space: state.color_space,
            brightness_nits: hdr.brightness_nits,
            expand_gamut: hdr.expand_gamut,
            ..Default::default()
        };
        *last_frame = Some(now);

        let input = VulkanImage {
            image: source.image,
            size: Size::new(source.extent.width, source.extent.height),
            format: source.view_format,
        };
        let viewport = Viewport {
            x: r.offset.x as f32,
            y: r.offset.y as f32,
            mvp: None,
            output: VulkanImage {
                image,
                size: Size::new(state.extent.width, state.extent.height),
                format: state.output_format,
            },
            size: Size::new(r.extent.width, r.extent.height),
        };
        if let Err(e) = active.chain.frame(&input, &viewport, cmd, *frame_count, Some(&options)) {
            log_error!("filter chain frame failed, disabling: {e}");
            *failed = true;
        }

        // 5. Back to PRESENT_SRC for the presentation engine.
        d.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier(
                image,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::ImageLayout::PRESENT_SRC_KHR,
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                vk::AccessFlags::empty(),
            )],
        );
        d.end_command_buffer(cmd)?;

        let mut cmds = Vec::with_capacity(2);
        if let Some(init) = pending_init.take() {
            cmds.push(init.1);
            slot.init = Some(init);
        }
        cmds.push(cmd);
        let stages = vec![vk::PipelineStageFlags::ALL_COMMANDS; wait_semaphores.len()];
        let signal = [state.semaphores[image_index as usize]];
        let submit = vk::SubmitInfo::default()
            .wait_semaphores(wait_semaphores)
            .wait_dst_stage_mask(&stages)
            .command_buffers(&cmds)
            .signal_semaphores(&signal);

        d.reset_fences(&[slot.fence])?;
        if let Err(e) = d.queue_submit(queue, &[submit], slot.fence) {
            // Not submitted: keep the upload for the next attempt.
            if let Some(init) = slot.init.take() {
                *pending_init = Some(init);
            }
            return Err(e);
        }
        *frame_count += 1;
        Ok(Some(signal[0]))
    }
}
