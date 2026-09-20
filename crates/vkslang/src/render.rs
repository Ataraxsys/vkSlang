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

/// The parts of a `total`-sized image left outside `picture`.
fn bar_rects(picture: vk::Rect2D, total: vk::Extent2D) -> Vec<vk::Rect2D> {
    let rect = |x: i32, y: i32, w: u32, h: u32| vk::Rect2D {
        offset: vk::Offset2D { x, y },
        extent: vk::Extent2D { width: w, height: h },
    };
    let (px, py) = (picture.offset.x.max(0), picture.offset.y.max(0));
    let (pw, ph) = (picture.extent.width, picture.extent.height);
    let right = (px + pw as i32).clamp(0, total.width as i32);
    let bottom = (py + ph as i32).clamp(0, total.height as i32);
    [
        rect(0, 0, px as u32, total.height),
        rect(right, 0, total.width.saturating_sub(right as u32), total.height),
        rect(px, 0, pw, py as u32),
        rect(px, bottom, pw, total.height.saturating_sub(bottom as u32)),
    ]
    .into_iter()
    .filter(|r| r.extent.width > 0 && r.extent.height > 0)
    .collect()
}

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

fn warn_mismatch(preset: ColorSpace, state: &SwapchainState) {
    match vkslang_ipc::color_space_warning(to_ipc(preset), to_ipc(state.color_space), state.promoted) {
        Some(why) => log_warn!("{why}"),
        None if state.promoted => log_info!("HDR10 output: the game renders SDR, the preset writes HDR"),
        None => {}
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

/// Creates a device-local image and binds memory for it.
unsafe fn create_image(
    dev: &DeviceData,
    format: vk::Format,
    extent: vk::Extent2D,
    flags: vk::ImageCreateFlags,
    usage: vk::ImageUsageFlags,
) -> Result<(vk::Image, vk::DeviceMemory), vk::Result> {
    let d = &dev.fns;
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
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED),
        None,
    )?;
    let reqs = d.get_image_memory_requirements(image);
    let props = dev.instance.fns.get_physical_device_memory_properties(dev.physical_device);
    let type_index = (0..props.memory_type_count).find(|&i| {
        reqs.memory_type_bits & (1 << i) != 0
            && props.memory_types[i as usize]
                .property_flags
                .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
    });
    let Some(type_index) = type_index else {
        d.destroy_image(image, None);
        return Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
    };
    let memory = match d.allocate_memory(
        &vk::MemoryAllocateInfo::default().allocation_size(reqs.size).memory_type_index(type_index),
        None,
    ) {
        Ok(m) => m,
        Err(e) => {
            d.destroy_image(image, None);
            return Err(e);
        }
    };
    if let Err(e) = d.bind_image_memory(image, memory, 0) {
        d.destroy_image(image, None);
        d.free_memory(memory, None);
        return Err(e);
    }
    Ok((image, memory))
}

struct SourceImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    extent: vk::Extent2D,
    /// View format librashader uses to sample it (always non-sRGB, like a
    /// RetroArch core framebuffer).
    view_format: vk::Format,
    /// Region of the swapchain image holding the game picture.
    rect: vk::Rect2D,
    /// Region the preset draws into: differs from `rect` when the picture is
    /// stretched (square pixels rendered into a 4:3 area, for instance).
    display: vk::Rect2D,
    filter: vk::Filter,
}

impl SourceImage {
    unsafe fn new(
        dev: &DeviceData,
        format: vk::Format,
        swapchain_extent: vk::Extent2D,
        source: &Source,
    ) -> Result<SourceImage, vk::Result> {
        let rect = source.picture_rect(swapchain_extent);
        let display = source.display_rect(swapchain_extent);
        let extent = source.size_for(rect.extent);
        let view_format = srgb_to_unorm(format).unwrap_or(format);

        let mut flags = vk::ImageCreateFlags::empty();
        if view_format != format {
            // Blit sRGB->sRGB keeps the encoded bytes; the chain then samples
            // them through a UNORM view, like RetroArch does.
            flags |= vk::ImageCreateFlags::MUTABLE_FORMAT;
        }
        // TRANSFER_SRC: librashader copies Original into its history.
        let usage = vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::SAMPLED;
        let (image, memory) = create_image(dev, format, extent, flags, usage)?;
        log_debug!(
            "source {}x{} {:?} from picture {:?}, drawn into {:?} of {}x{}",
            extent.width,
            extent.height,
            format,
            rect,
            display,
            swapchain_extent.width,
            swapchain_extent.height
        );
        Ok(SourceImage { image, memory, extent, view_format, rect, display, filter: source.filter })
    }

    unsafe fn destroy(self, dev: &DeviceData) {
        dev.fns.destroy_image(self.image, None);
        dev.fns.free_memory(self.memory, None);
    }
}

pub struct SwapchainPlan {
    pub extent: vk::Extent2D,
    /// Format the swapchain images were actually created with.
    pub format: vk::Format,
    /// Format the application renders through (differs once promoted).
    pub app_format: vk::Format,
    /// Format librashader renders through.
    pub output_format: vk::Format,
    pub color_space: vk::ColorSpaceKHR,
    /// The layer turned an SDR swapchain into an HDR10 one and the pixels
    /// must be copied through the application's format (staging image).
    pub promoted: bool,
    /// The layer promoted the swapchain to HDR10 (with or without staging).
    pub hdr_promoted: bool,
}

pub struct SwapchainState {
    images: Vec<vk::Image>,
    /// Signalled by our submission, waited on by the real present. Indexed by
    /// image index: reacquiring an image guarantees its previous present
    /// consumed the semaphore.
    semaphores: Vec<vk::Semaphore>,
    format: vk::Format,
    /// Format of the pixels the chain reads (the application's format on a
    /// promoted swapchain).
    source_format: vk::Format,
    extent: vk::Extent2D,
    /// Format librashader renders to (UNORM view of an sRGB swapchain when the
    /// device allows mutable swapchain formats).
    output_format: vk::Format,
    color_space: ColorSpace,
    /// Low-resolution copy fed to the chain as `Original`. `None` if it could
    /// not be created with the current settings (frames pass through).
    source: Option<SourceImage>,
    /// The layer promoted this swapchain to HDR10.
    promoted: bool,
    /// Opaque black, for the bars around a smaller picture area.
    black: Option<BlackImage>,
    /// On a promoted (HDR10) swapchain, a full-size image in the
    /// application's own format: the swapchain images hold 8-bit SDR pixels
    /// written through the application's view, so they are copied raw here
    /// before being scaled and read as SDR by the filter chain.
    staging: Option<StagingImage>,
}

/// Readback target for [`Request::Capture`]: an RGBA8 copy of the picture
/// the application drew, before the preset.
struct CaptureTarget {
    image: vk::Image,
    memory: vk::DeviceMemory,
    buffer: vk::Buffer,
    buffer_memory: vk::DeviceMemory,
    /// Mapped pointer kept as an address: `Runtime` travels between threads
    /// (it lives behind the device mutex) and a raw pointer is not `Send`.
    mapped: usize,
    extent: vk::Extent2D,
}

impl CaptureTarget {
    unsafe fn new(dev: &DeviceData, extent: vk::Extent2D) -> Result<CaptureTarget, vk::Result> {
        let d = &dev.fns;
        let (image, memory) = create_image(
            dev,
            vk::Format::R8G8B8A8_UNORM,
            extent,
            vk::ImageCreateFlags::empty(),
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC,
        )?;
        let size = u64::from(extent.width) * u64::from(extent.height) * 4;
        let buffer = d.create_buffer(
            &vk::BufferCreateInfo::default().size(size).usage(vk::BufferUsageFlags::TRANSFER_DST),
            None,
        )?;
        let reqs = d.get_buffer_memory_requirements(buffer);
        let props = dev.instance.fns.get_physical_device_memory_properties(dev.physical_device);
        let wanted = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let type_index = (0..props.memory_type_count).find(|&i| {
            reqs.memory_type_bits & (1 << i) != 0
                && props.memory_types[i as usize].property_flags.contains(wanted)
        });
        let cleanup = |e: vk::Result| {
            d.destroy_buffer(buffer, None);
            d.destroy_image(image, None);
            d.free_memory(memory, None);
            e
        };
        let Some(type_index) = type_index else {
            return Err(cleanup(vk::Result::ERROR_OUT_OF_HOST_MEMORY));
        };
        let buffer_memory = d
            .allocate_memory(
                &vk::MemoryAllocateInfo::default().allocation_size(reqs.size).memory_type_index(type_index),
                None,
            )
            .map_err(cleanup)?;
        d.bind_buffer_memory(buffer, buffer_memory, 0).map_err(cleanup)?;
        let mapped = d
            .map_memory(buffer_memory, 0, size, vk::MemoryMapFlags::empty())
            .map_err(cleanup)? as usize;
        Ok(CaptureTarget { image, memory, buffer, buffer_memory, mapped, extent })
    }

    unsafe fn destroy(self, dev: &DeviceData) {
        let d = &dev.fns;
        d.unmap_memory(self.buffer_memory);
        d.destroy_buffer(self.buffer, None);
        d.free_memory(self.buffer_memory, None);
        d.destroy_image(self.image, None);
        d.free_memory(self.memory, None);
    }

    /// Writes the mapped pixels next to the control socket.
    unsafe fn write(
        &self,
        base: vk::Extent2D,
        area: vk::Rect2D,
    ) -> std::io::Result<vkslang_ipc::Capture> {
        use std::io::Write;
        let dir = vkslang_ipc::socket_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}-capture.bin", std::process::id()));
        let len = (self.extent.width * self.extent.height * 4) as usize;
        let pixels = std::slice::from_raw_parts(self.mapped as *const u8, len);
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path)?);
        file.write_all(&vkslang_ipc::CAPTURE_MAGIC)?;
        file.write_all(&self.extent.width.to_le_bytes())?;
        file.write_all(&self.extent.height.to_le_bytes())?;
        file.write_all(pixels)?;
        file.flush()?;
        Ok(vkslang_ipc::Capture {
            path: path.display().to_string(),
            size: [self.extent.width, self.extent.height],
            base: [base.width, base.height],
            area: [
                area.offset.x,
                area.offset.y,
                area.extent.width as i32,
                area.extent.height as i32,
            ],
            id: 0,
        })
    }
}

/// 1x1 image cleared to opaque black, blitted into the letterbox bars.
///
/// librashader clears the whole output to *transparent* black before drawing
/// into the viewport, and a compositor takes that alpha seriously (the bars
/// show up white under KWin), so the bars are repainted afterwards.
struct BlackImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
}

impl BlackImage {
    unsafe fn new(dev: &DeviceData, format: vk::Format) -> Result<BlackImage, vk::Result> {
        let (image, memory) = create_image(
            dev,
            format,
            vk::Extent2D { width: 1, height: 1 },
            vk::ImageCreateFlags::empty(),
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC,
        )?;
        Ok(BlackImage { image, memory })
    }

    unsafe fn destroy(self, dev: &DeviceData) {
        dev.fns.destroy_image(self.image, None);
        dev.fns.free_memory(self.memory, None);
    }
}

struct StagingImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
}

impl StagingImage {
    unsafe fn new(
        dev: &DeviceData,
        format: vk::Format,
        extent: vk::Extent2D,
    ) -> Result<StagingImage, vk::Result> {
        let (image, memory) = create_image(
            dev,
            format,
            extent,
            vk::ImageCreateFlags::empty(),
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC,
        )?;
        Ok(StagingImage { image, memory })
    }

    unsafe fn destroy(self, dev: &DeviceData) {
        dev.fns.destroy_image(self.image, None);
        dev.fns.free_memory(self.memory, None);
    }
}

impl SwapchainState {
    /// `images` are the swapchain images returned by the next layer.
    pub unsafe fn new(
        dev: &DeviceData,
        plan: &SwapchainPlan,
        images: Vec<vk::Image>,
    ) -> Result<SwapchainState, vk::Result> {
        let mut state = SwapchainState {
            semaphores: Vec::with_capacity(images.len()),
            images,
            format: plan.format,
            // Pixels read by the chain are the ones the application wrote.
            source_format: if plan.promoted { plan.app_format } else { plan.format },
            extent: plan.extent,
            output_format: plan.output_format,
            color_space: color_space(plan.color_space),
            promoted: plan.promoted || plan.hdr_promoted,
            source: None,
            staging: None,
            black: None,
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
        match BlackImage::new(dev, plan.format) {
            Ok(black) => state.black = Some(black),
            Err(e) => log_warn!("cannot create the bar fill image: {e}"),
        }
        if plan.promoted {
            match StagingImage::new(dev, state.source_format, state.extent) {
                Ok(staging) => state.staging = Some(staging),
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
        match SourceImage::new(dev, self.source_format, self.extent, source) {
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
        if let Some(staging) = self.staging.take() {
            staging.destroy(dev);
        }
        if let Some(black) = self.black.take() {
            black.destroy(dev);
        }
    }
}

// ------------------------------------------------------------------ runtime

struct FrameSlot {
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    /// Signalled by our own vkAcquireNextImageKHR for a subframe; safe to
    /// reuse once this slot's fence has been waited on.
    acquire: vk::Semaphore,
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
    /// Last frame the application drew (subframes excluded).
    last_frame: Option<Instant>,
    /// Rates are counted over a window: averaging the inverse of each
    /// interval would be skewed by the subframes, presented back to back.
    rate_window: Instant,
    frames_in_window: u32,
    presents_in_window: u32,
    capture: Option<CaptureTarget>,
    /// Application frame rate, bound as the `FPS` uniform.
    fps: f32,
    /// Presentations per second (subframes included).
    present_fps: f32,
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
            rate_window: Instant::now(),
            frames_in_window: 0,
            presents_in_window: 0,
            capture: None,
            fps: 60.0,
            present_fps: 60.0,
        };
        for _ in 0..RING {
            let slot = allocate_cmd(dev, pool).and_then(|cmd| {
                let fence = d.create_fence(
                    &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                    None,
                )?;
                let acquire = d.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?;
                Ok(FrameSlot { cmd, fence, init: None, acquire })
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
            warn_mismatch(loaded.color_space, state);
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

        let window = self.rate_window.elapsed().as_secs_f32();
        if window >= 0.5 {
            self.fps = self.frames_in_window as f32 / window;
            self.present_fps = self.presents_in_window as f32 / window;
            self.frames_in_window = 0;
            self.presents_in_window = 0;
            self.rate_window = Instant::now();
        }

        let mut ctl = control();
        self.enabled = ctl.enabled;
        self.hdr = ctl.hdr;
        ctl.source_fps = self.fps;
        ctl.present_fps = self.present_fps;

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
            // The input size changed, so the published sizes must follow.
            self.publish_outputs();
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
                picture: s
                    .source
                    .as_ref()
                    .map_or([s.extent.width, s.extent.height], |src| {
                        [src.rect.extent.width, src.rect.extent.height]
                    }),
                input: s
                    .source
                    .as_ref()
                    .map_or([0, 0], |src| [src.extent.width, src.extent.height]),
                format: format!("{:?}", s.format),
                color_space: to_ipc(s.color_space),
                promoted: s.promoted,
            })
            .collect();
    }

    pub fn track_swapchain(&mut self, swapchain: vk::SwapchainKHR, state: SwapchainState) {
        if let Some(active) = &self.chain {
            warn_mismatch(active.color_space, &state);
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
        if let Some(capture) = self.capture.take() {
            capture.destroy(dev);
        }
        for slot in &mut self.slots {
            if let Some((pool, _)) = slot.init.take() {
                dev.fns.destroy_command_pool(pool, None);
            }
            dev.fns.destroy_fence(slot.fence, None);
            dev.fns.destroy_semaphore(slot.acquire, None);
        }
        // Destroying the pool frees every command buffer allocated from it.
        dev.fns.destroy_command_pool(self.pool, None);
        control().outputs.clear();
    }

    /// Presents the same application frame `total - 1` more times, so
    /// presets can alternate fields (interlacing) or insert black frames
    /// faster than the application's frame rate.
    ///
    /// The layer acquires images of its own, which is why the swapchain was
    /// created with extras. Anything unexpected (no image available in time,
    /// a resize) simply ends the extra presentations for this frame.
    pub unsafe fn present_subframes(
        &mut self,
        dev: &DeviceData,
        queue: vk::Queue,
        swapchain: vk::SwapchainKHR,
        total: u32,
        black: bool,
    ) {
        if total <= 1 || !self.is_rendering() || !self.swapchains.contains_key(&swapchain) {
            return;
        }
        let d = &dev.fns;
        for current in 2..=total {
            // The slot render() will use: waiting on its fence here frees its
            // acquire semaphore before we reuse it.
            let slot = self.next_slot;
            if d.wait_for_fences(&[self.slots[slot].fence], true, u64::MAX).is_err() {
                return;
            }
            let acquire = self.slots[slot].acquire;
            let mut index = 0;
            let r = (dev.swapchain_fn.acquire_next_image_khr)(
                dev.handle,
                swapchain,
                50_000_000, // 50 ms: never hold the application's loop hostage
                acquire,
                vk::Fence::null(),
                &mut index,
            );
            if r != vk::Result::SUCCESS && r != vk::Result::SUBOPTIMAL_KHR {
                // Every image is busy: skip the rest of this frame's
                // subframes rather than delay the application.
                log_debug!("no free image for subframe {current}/{total} ({r})");
                return;
            }

            let done = if black {
                self.present_black(dev, queue, swapchain, index, acquire)
            } else {
                self.render(dev, queue, swapchain, index, &[acquire], Some((current, total)))
                    .unwrap_or(None)
            };
            let Some(done) = done else { return };

            let wait = [done];
            let swapchains = [swapchain];
            let indices = [index];
            let info = vk::PresentInfoKHR::default()
                .wait_semaphores(&wait)
                .swapchains(&swapchains)
                .image_indices(&indices);
            let r = (dev.swapchain_fn.queue_present_khr)(queue, &info);
            if r != vk::Result::SUCCESS && r != vk::Result::SUBOPTIMAL_KHR {
                log_debug!("subframe present failed ({r})");
                return;
            }
        }
    }

    /// Cheap black frame insertion: clear the acquired image, no shader.
    unsafe fn present_black(
        &mut self,
        dev: &DeviceData,
        queue: vk::Queue,
        swapchain: vk::SwapchainKHR,
        image_index: u32,
        acquire: vk::Semaphore,
    ) -> Option<vk::Semaphore> {
        self.presents_in_window += 1;

        let state = self.swapchains.get(&swapchain)?;
        let image = *state.images.get(image_index as usize)?;
        let signal = [*state.semaphores.get(image_index as usize)?];
        let d = &dev.fns;

        let slot = &mut self.slots[self.next_slot];
        self.next_slot = (self.next_slot + 1) % RING;
        let cmd = slot.cmd;
        d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty()).ok()?;
        d.begin_command_buffer(
            cmd,
            &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )
        .ok()?;
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let barrier = |old, new, src, dst| {
            vk::ImageMemoryBarrier::default()
                .image(image)
                .old_layout(old)
                .new_layout(new)
                .src_access_mask(src)
                .dst_access_mask(dst)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .subresource_range(range)
        };
        d.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier(
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::AccessFlags::empty(),
                vk::AccessFlags::TRANSFER_WRITE,
            )],
        );
        d.cmd_clear_color_image(
            cmd,
            image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &vk::ClearColorValue { float32: [0.0, 0.0, 0.0, 1.0] },
            &[range],
        );
        d.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier(
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::PRESENT_SRC_KHR,
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::empty(),
            )],
        );
        d.end_command_buffer(cmd).ok()?;

        let wait = [acquire];
        let stages = [vk::PipelineStageFlags::ALL_COMMANDS];
        let cmds = [cmd];
        let submit = vk::SubmitInfo::default()
            .wait_semaphores(&wait)
            .wait_dst_stage_mask(&stages)
            .command_buffers(&cmds)
            .signal_semaphores(&signal);
        d.reset_fences(&[slot.fence]).ok()?;
        d.queue_submit(queue, &[submit], slot.fence).ok()?;
        Some(signal[0])
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
        // `(current, total)` when rendering an extra presentation of the
        // same application frame.
        subframe: Option<(u32, u32)>,
    ) -> Result<Option<vk::Semaphore>, vk::Result> {
        if !self.is_rendering() {
            return Ok(None);
        }
        let self_fps_value = self.fps;
        let Runtime {
            slots,
            next_slot,
            chain,
            failed,
            hdr,
            pending_init,
            swapchains,
            frame_count,
            last_frame,
            frames_in_window,
            presents_in_window,
            capture,
            ..
        } = self;
        let (Some(state), Some(active)) = (swapchains.get(&swapchain), chain.as_mut()) else {
            return Ok(None);
        };
        let (Some(&image), Some(source)) = (state.images.get(image_index as usize), state.source.as_ref()) else {
            return Ok(None);
        };
        let d = &dev.fns;
        let self_fps = self_fps_value;

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

        let layers = vk::ImageSubresourceLayers::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .layer_count(1);
        let r = source.rect;
        // Set when a capture was recorded into this frame's commands.
        let mut captured = None;

        if subframe.is_none() {
            // 1. swapchain -> TRANSFER_SRC, source -> TRANSFER_DST (previous
            //    contents discarded; the chain keeps its own history copies).
            let mut first = vec![
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
            ];
            if let Some(staging) = &state.staging {
                first.push(barrier(
                    staging.image,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::TRANSFER_WRITE,
                ));
            }
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &first,
            );

            // 2. On a promoted (HDR10) swapchain, the application's pixels are
            //    8-bit SDR written through its own view: copy them raw (format
            //    classes are compatible) so they can be read as SDR.
            let picture = match &state.staging {
                Some(staging) => {
                    d.cmd_copy_image(
                        cmd,
                        image,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        staging.image,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[vk::ImageCopy::default()
                            .src_subresource(layers)
                            .dst_subresource(layers)
                            .extent(vk::Extent3D {
                                width: state.extent.width,
                                height: state.extent.height,
                                depth: 1,
                            })],
                    );
                    d.cmd_pipeline_barrier(
                        cmd,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[barrier(
                            staging.image,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            vk::AccessFlags::TRANSFER_WRITE,
                            vk::AccessFlags::TRANSFER_READ,
                        )],
                    );
                    staging.image
                }
                None => image,
            };

            // 2a. Capture requested by vkslang-ui: an RGBA8 copy of the picture as
        //     the application drew it, before the preset touches it.
        if subframe.is_none() {
            if let Some(max_width) = control().capture_request {
                // The whole image, so the UI can also frame a new picture
                // area outside the current one.
                let rect = vk::Rect2D { offset: vk::Offset2D::default(), extent: state.extent };
                let scale = (max_width as f32 / rect.extent.width as f32).min(1.0);
                let extent = vk::Extent2D {
                    width: ((rect.extent.width as f32 * scale).round() as u32).max(1),
                    height: ((rect.extent.height as f32 * scale).round() as u32).max(1),
                };
                if capture.as_ref().is_none_or(|c| c.extent != extent) {
                    if let Some(old) = capture.take() {
                        old.destroy(dev);
                    }
                    match CaptureTarget::new(dev, extent) {
                        Ok(target) => *capture = Some(target),
                        Err(e) => log_error!("cannot create the capture target: {e}"),
                    }
                }
                if let Some(target) = capture.as_ref() {
                    let barrier_capture = |old, new, src, dst| {
                        vk::ImageMemoryBarrier::default()
                            .image(target.image)
                            .old_layout(old)
                            .new_layout(new)
                            .src_access_mask(src)
                            .dst_access_mask(dst)
                            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .subresource_range(color)
                    };
                    d.cmd_pipeline_barrier(
                        cmd,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[barrier_capture(
                            vk::ImageLayout::UNDEFINED,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            vk::AccessFlags::empty(),
                            vk::AccessFlags::TRANSFER_WRITE,
                        )],
                    );
                    d.cmd_blit_image(
                        cmd,
                        picture,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        target.image,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[vk::ImageBlit::default()
                            .src_subresource(layers)
                            .src_offsets([
                                vk::Offset3D { x: rect.offset.x, y: rect.offset.y, z: 0 },
                                vk::Offset3D {
                                    x: rect.offset.x + rect.extent.width as i32,
                                    y: rect.offset.y + rect.extent.height as i32,
                                    z: 1,
                                },
                            ])
                            .dst_subresource(layers)
                            .dst_offsets([
                                vk::Offset3D::default(),
                                vk::Offset3D { x: extent.width as i32, y: extent.height as i32, z: 1 },
                            ])],
                        vk::Filter::LINEAR,
                    );
                    d.cmd_pipeline_barrier(
                        cmd,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[barrier_capture(
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            vk::AccessFlags::TRANSFER_WRITE,
                            vk::AccessFlags::TRANSFER_READ,
                        )],
                    );
                    d.cmd_copy_image_to_buffer(
                        cmd,
                        target.image,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        target.buffer,
                        &[vk::BufferImageCopy::default()
                            .image_subresource(layers)
                            .image_extent(vk::Extent3D {
                                width: extent.width,
                                height: extent.height,
                                depth: 1,
                            })],
                    );
                    captured = Some((rect.extent, source.rect));
                }
            }
        }

        // 2b. Downsample the picture region to the logical source resolution.
            if r.extent == source.extent {
                d.cmd_copy_image(
                    cmd,
                    picture,
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
                let sz = source.extent;
                d.cmd_blit_image(
                    cmd,
                    picture,
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
                            vk::Offset3D { x: sz.width as i32, y: sz.height as i32, z: 1 },
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

        } else {
            // Subframe: the source image still holds this frame's picture and
            // is still in SHADER_READ_ONLY, so only the freshly acquired image
            // needs a layout; its previous contents are discarded.
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier(
                    image,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::COLOR_ATTACHMENT_READ | vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                )],
            );
        }

        // 4. Run the preset. The viewport is the picture region, so pillar/
        //    letterbox bars are cleared to black by librashader's final pass.
        let now = Instant::now();
        // The application's own rhythm: subframes must not shorten it, or the
        // shaders would believe the game runs three times faster.
        let elapsed = last_frame.map(|t| now.duration_since(t));
        if subframe.is_none() {
            *frames_in_window += 1;
        }
        *presents_in_window += 1;
        let options = FrameOptions {
            frametime_delta: elapsed.map_or(0, |e| e.as_millis() as u32),
            frames_per_second: self_fps,
            // Binds HDRMode / BrightnessNits / ExpandGamut for HDR presets.
            color_space: state.color_space,
            brightness_nits: hdr.brightness_nits,
            expand_gamut: hdr.expand_gamut,
            // Bound as CurrentSubFrame / TotalSubFrames; FrameCount also
            // advances per subframe, so presets that alternate fields on it
            // interlace at the presentation rate rather than the game's.
            current_subframe: subframe.map_or(1, |(current, _)| current),
            total_subframes: subframe.map_or(1, |(_, total)| total),
            ..Default::default()
        };
        if subframe.is_none() {
            *last_frame = Some(now);
        }

        let input = VulkanImage {
            image: source.image,
            size: Size::new(source.extent.width, source.extent.height),
            format: source.view_format,
        };
        let display = source.display;
        let viewport = Viewport {
            x: display.offset.x as f32,
            y: display.offset.y as f32,
            mvp: None,
            output: VulkanImage {
                image,
                size: Size::new(state.extent.width, state.extent.height),
                format: state.output_format,
            },
            size: Size::new(display.extent.width, display.extent.height),
        };
        if let Err(e) = active.chain.frame(&input, &viewport, cmd, *frame_count, Some(&options)) {
            log_error!("filter chain frame failed, disabling: {e}");
            *failed = true;
        }

        // 5. Repaint the letterbox bars opaque black (the chain cleared them
        //    to transparent black, which a compositor shows as garbage).
        let bars = bar_rects(display, state.extent);
        let mut layout = vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL;
        if let (false, Some(black)) = (bars.is_empty(), state.black.as_ref()) {
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[
                    barrier(
                        image,
                        layout,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                        vk::AccessFlags::TRANSFER_WRITE,
                    ),
                    barrier(
                        black.image,
                        vk::ImageLayout::UNDEFINED,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        vk::AccessFlags::empty(),
                        vk::AccessFlags::TRANSFER_WRITE,
                    ),
                ],
            );
            layout = vk::ImageLayout::TRANSFER_DST_OPTIMAL;
            d.cmd_clear_color_image(
                cmd,
                black.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &vk::ClearColorValue { float32: [0.0, 0.0, 0.0, 1.0] },
                &[color],
            );
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier(
                    black.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::AccessFlags::TRANSFER_WRITE,
                    vk::AccessFlags::TRANSFER_READ,
                )],
            );
            let blits: Vec<vk::ImageBlit> = bars
                .iter()
                .map(|bar| {
                    vk::ImageBlit::default()
                        .src_subresource(layers)
                        .src_offsets([
                            vk::Offset3D::default(),
                            vk::Offset3D { x: 1, y: 1, z: 1 },
                        ])
                        .dst_subresource(layers)
                        .dst_offsets([
                            vk::Offset3D { x: bar.offset.x, y: bar.offset.y, z: 0 },
                            vk::Offset3D {
                                x: bar.offset.x + bar.extent.width as i32,
                                y: bar.offset.y + bar.extent.height as i32,
                                z: 1,
                            },
                        ])
                })
                .collect();
            d.cmd_blit_image(
                cmd,
                black.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &blits,
                vk::Filter::NEAREST,
            );
        }

        // 6. Back to PRESENT_SRC for the presentation engine.
        d.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT | vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier(
                image,
                layout,
                vk::ImageLayout::PRESENT_SRC_KHR,
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE | vk::AccessFlags::TRANSFER_WRITE,
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

        // The capture is read back once the frame has run, which costs one
        // wait but only on the frame the UI asked for.
        if let (Some((base, area)), Some(target)) = (captured, capture.as_ref()) {
            let fence = slot.fence;
            if d.wait_for_fences(&[fence], true, 1_000_000_000).is_ok() {
                let mut ctl = control();
                let id = ctl.capture.as_ref().map_or(1, |c| c.id + 1);
                match target.write(base, area) {
                    Ok(mut info) => {
                        info.id = id;
                        log_debug!("capture {}x{} -> {}", info.size[0], info.size[1], info.path);
                        ctl.capture = Some(info);
                    }
                    Err(e) => log_error!("cannot write the capture: {e}"),
                }
                ctl.capture_request = None;
            }
        }
        Ok(Some(signal[0]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_around_a_pillarboxed_picture() {
        let total = vk::Extent2D { width: 3840, height: 2160 };
        let picture = vk::Rect2D {
            offset: vk::Offset2D { x: 480, y: 0 },
            extent: vk::Extent2D { width: 2880, height: 2160 },
        };
        let bars = bar_rects(picture, total);
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].extent, vk::Extent2D { width: 480, height: 2160 });
        assert_eq!(bars[1].offset.x, 3360);
        assert_eq!(bars[1].extent, vk::Extent2D { width: 480, height: 2160 });
    }

    #[test]
    fn no_bars_when_the_picture_fills_the_image() {
        let total = vk::Extent2D { width: 1920, height: 1080 };
        let picture = vk::Rect2D { offset: vk::Offset2D::default(), extent: total };
        assert!(bar_rects(picture, total).is_empty());
    }
}
