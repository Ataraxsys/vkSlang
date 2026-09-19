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

use crate::config::{self, Config};
use crate::state::DeviceData;
use crate::{log_debug, log_error, log_info, log_warn};
use ash::vk;
use librashader::presets::{ShaderFeatures, ShaderPreset};
use librashader::runtime::vk::{FilterChain, FilterChainOptions, FrameOptions, VulkanImage};
use librashader::runtime::{ColorSpace, FilterChainParameters, Size, Viewport};
use std::collections::HashMap;
use std::ffi::c_void;
use std::time::Instant;

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

fn color_space(cs: vk::ColorSpaceKHR) -> ColorSpace {
    match cs {
        vk::ColorSpaceKHR::HDR10_ST2084_EXT => ColorSpace::Hdr10,
        vk::ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT => ColorSpace::ScRgb,
        _ => ColorSpace::Sdr,
    }
}

struct FrameSlot {
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    /// One-shot filter chain upload (LUTs...) submitted with this slot.
    init_cmd: Option<vk::CommandBuffer>,
}

pub struct SwapchainState {
    images: Vec<vk::Image>,
    /// Signalled by our submission, waited on by the real present. Indexed by
    /// image index: reacquiring an image guarantees its previous present
    /// consumed the semaphore.
    semaphores: Vec<vk::Semaphore>,
    extent: vk::Extent2D,
    /// Format librashader renders to (UNORM view of an sRGB swapchain when the
    /// device allows mutable swapchain formats).
    output_format: vk::Format,
    color_space: ColorSpace,
    /// Region of the swapchain image holding the game picture.
    src_rect: vk::Rect2D,
    /// Low-resolution copy fed to the chain as `Original`.
    source: vk::Image,
    source_memory: vk::DeviceMemory,
    source_extent: vk::Extent2D,
    /// View format librashader uses to sample `source` (always non-sRGB, like
    /// a RetroArch core framebuffer).
    source_view_format: vk::Format,
}

pub struct Runtime {
    pool: vk::CommandPool,
    slots: Vec<FrameSlot>,
    next_slot: usize,
    chain: Option<FilterChain>,
    /// Set after a load or frame error: presents pass through from then on.
    disabled: bool,
    pending_init: Option<vk::CommandBuffer>,
    swapchains: HashMap<vk::SwapchainKHR, SwapchainState>,
    frame_count: usize,
    last_frame: Option<Instant>,
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

impl Runtime {
    pub unsafe fn new(dev: &DeviceData) -> Result<Runtime, vk::Result> {
        let d = &dev.fns;
        let pool = d.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                .queue_family_index(dev.queue_family),
            None,
        )?;
        let mut rt = Runtime {
            pool,
            slots: Vec::with_capacity(RING),
            next_slot: 0,
            chain: None,
            disabled: false,
            pending_init: None,
            swapchains: HashMap::new(),
            frame_count: 0,
            last_frame: None,
        };
        for _ in 0..RING {
            let cmd = allocate_cmd(dev, pool)?;
            let fence = d.create_fence(
                &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                None,
            )?;
            rt.slots.push(FrameSlot { cmd, fence, init_cmd: None });
        }
        Ok(rt)
    }

    /// Loads the preset once per device. Shader compilation happens here (it
    /// can take seconds on big presets); GPU uploads are deferred to the first
    /// present so no queue is touched outside of vkQueuePresentKHR.
    pub unsafe fn ensure_chain(&mut self, dev: &DeviceData, cfg: &Config) {
        if self.chain.is_some() || self.disabled {
            return;
        }
        let Some(path) = cfg.preset.as_ref() else { return };
        let started = Instant::now();

        let result = (|| -> Result<(FilterChain, vk::CommandBuffer), String> {
            let preset = ShaderPreset::try_parse(path, ShaderFeatures::NONE).map_err(|e| e.to_string())?;
            let cmd = allocate_cmd(dev, self.pool).map_err(|e| e.to_string())?;
            let fail = |e: String| {
                dev.fns.free_command_buffers(self.pool, &[cmd]);
                e
            };
            dev.fns
                .begin_command_buffer(
                    cmd,
                    &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|e| fail(e.to_string()))?;
            let options = FilterChainOptions {
                // One extra frame of slack before librashader recycles
                // per-frame image views/framebuffers.
                frames_in_flight: RING as u32 + 1,
                force_no_mipmaps: false,
                // Dynamic rendering is only usable if the *application*
                // enabled the feature; render passes always work.
                use_dynamic_rendering: false,
                disable_cache: false,
            };
            let vulkan = (dev.physical_device, dev.instance.fns.clone(), dev.fns.clone(), dev.queue);
            let chain = FilterChain::load_from_preset_deferred(preset, vulkan, cmd, Some(&options))
                .map_err(|e| fail(e.to_string()))?;
            dev.fns.end_command_buffer(cmd).map_err(|e| fail(e.to_string()))?;
            Ok((chain, cmd))
        })();

        match result {
            Ok((chain, cmd)) => {
                for (name, value) in &cfg.params {
                    if chain.parameters().set_parameter_value(name, *value).is_none() {
                        log_warn!("preset has no parameter '{name}'");
                    }
                }
                log_info!("loaded {} in {:.2?}", path.display(), started.elapsed());
                self.chain = Some(chain);
                self.pending_init = Some(cmd);
            }
            Err(e) => {
                log_error!("failed to load preset {}: {e}", path.display());
                self.disabled = true;
            }
        }
    }

    pub fn is_rendering(&self) -> bool {
        self.chain.is_some() && !self.disabled
    }

    pub fn track_swapchain(&mut self, swapchain: vk::SwapchainKHR, state: SwapchainState) {
        self.swapchains.insert(swapchain, state);
    }

    unsafe fn wait_all(&self, dev: &DeviceData) {
        let fences: Vec<_> = self.slots.iter().map(|s| s.fence).collect();
        let _ = dev.fns.wait_for_fences(&fences, true, u64::MAX);
    }

    pub unsafe fn forget_swapchain(&mut self, dev: &DeviceData, swapchain: vk::SwapchainKHR) {
        if let Some(state) = self.swapchains.remove(&swapchain) {
            self.wait_all(dev);
            state.destroy(dev);
        }
    }

    pub unsafe fn destroy(mut self, dev: &DeviceData) {
        let _ = dev.fns.device_wait_idle();
        for (_, state) in self.swapchains.drain() {
            state.destroy(dev);
        }
        // Drops pipelines, images and the gpu-allocator while the device lives.
        self.chain = None;
        for slot in &self.slots {
            dev.fns.destroy_fence(slot.fence, None);
        }
        // Destroying the pool frees every command buffer allocated from it.
        dev.fns.destroy_command_pool(self.pool, None);
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
        let Runtime { pool, slots, next_slot, chain, disabled, pending_init, swapchains, frame_count, last_frame } = self;
        let (Some(state), Some(chain)) = (swapchains.get(&swapchain), chain.as_mut()) else {
            return Ok(None);
        };
        if *disabled {
            return Ok(None);
        }
        let Some(&image) = state.images.get(image_index as usize) else { return Ok(None) };
        let d = &dev.fns;

        let slot = &mut slots[*next_slot];
        *next_slot = (*next_slot + 1) % RING;
        d.wait_for_fences(&[slot.fence], true, u64::MAX)?;
        if let Some(init) = slot.init_cmd.take() {
            d.free_command_buffers(*pool, &[init]);
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
                    state.source,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::TRANSFER_WRITE,
                ),
            ],
        );

        // 2. Downsample the picture region to the logical source resolution.
        let r = state.src_rect;
        let layers = vk::ImageSubresourceLayers::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .layer_count(1);
        if r.extent == state.source_extent {
            d.cmd_copy_image(
                cmd,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                state.source,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[vk::ImageCopy::default()
                    .src_subresource(layers)
                    .src_offset(vk::Offset3D { x: r.offset.x, y: r.offset.y, z: 0 })
                    .dst_subresource(layers)
                    .extent(vk::Extent3D { width: r.extent.width, height: r.extent.height, depth: 1 })],
            );
        } else {
            let s = state.source_extent;
            d.cmd_blit_image(
                cmd,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                state.source,
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
                config::get().source_filter,
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
                    state.source,
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
            color_space: state.color_space,
            ..Default::default()
        };
        *last_frame = Some(now);

        let input = VulkanImage {
            image: state.source,
            size: Size::new(state.source_extent.width, state.source_extent.height),
            format: state.source_view_format,
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
        if let Err(e) = chain.frame(&input, &viewport, cmd, *frame_count, Some(&options)) {
            log_error!("filter chain frame failed, disabling: {e}");
            *disabled = true;
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
            cmds.push(init);
            slot.init_cmd = Some(init);
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
        d.queue_submit(queue, &[submit], slot.fence)?;
        *frame_count += 1;
        Ok(Some(signal[0]))
    }
}

impl SwapchainState {
    /// Creates the per-swapchain resources. `images` are the swapchain images
    /// returned by the next layer.
    pub unsafe fn new(
        dev: &DeviceData,
        info: &vk::SwapchainCreateInfoKHR,
        images: Vec<vk::Image>,
        output_format: vk::Format,
    ) -> Result<SwapchainState, vk::Result> {
        let cfg = config::get();
        let d = &dev.fns;
        let extent = info.image_extent;
        let src_rect = cfg.source_rect_for(extent);
        let source_extent = cfg.source_res.unwrap_or(src_rect.extent);
        let source_view_format = srgb_to_unorm(info.image_format).unwrap_or(info.image_format);

        let mut flags = vk::ImageCreateFlags::empty();
        if source_view_format != info.image_format {
            // Blit sRGB->sRGB keeps the encoded bytes; the chain then samples
            // them through a UNORM view, like RetroArch does.
            flags |= vk::ImageCreateFlags::MUTABLE_FORMAT;
        }
        let source = d.create_image(
            &vk::ImageCreateInfo::default()
                .flags(flags)
                .image_type(vk::ImageType::TYPE_2D)
                .format(info.image_format)
                .extent(vk::Extent3D { width: source_extent.width, height: source_extent.height, depth: 1 })
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

        let reqs = d.get_image_memory_requirements(source);
        let props = dev.instance.fns.get_physical_device_memory_properties(dev.physical_device);
        let Some(type_index) = (0..props.memory_type_count).find(|&i| {
            reqs.memory_type_bits & (1 << i) != 0
                && props.memory_types[i as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        }) else {
            d.destroy_image(source, None);
            return Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
        };
        let source_memory = match d.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(reqs.size)
                .memory_type_index(type_index),
            None,
        ) {
            Ok(m) => m,
            Err(e) => {
                d.destroy_image(source, None);
                return Err(e);
            }
        };

        let mut state = SwapchainState {
            semaphores: Vec::with_capacity(images.len()),
            images,
            extent,
            output_format,
            color_space: color_space(info.image_color_space),
            src_rect,
            source,
            source_memory,
            source_extent,
            source_view_format,
        };
        if let Err(e) = d.bind_image_memory(source, source_memory, 0) {
            state.destroy(dev);
            return Err(e);
        }
        for _ in 0..state.images.len() {
            match d.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) {
                Ok(s) => state.semaphores.push(s),
                Err(e) => {
                    state.destroy(dev);
                    return Err(e);
                }
            }
        }
        log_debug!(
            "swapchain {}x{} {:?} -> picture {:?}, source {}x{}",
            extent.width, extent.height, info.image_format, src_rect, source_extent.width, source_extent.height
        );
        Ok(state)
    }

    unsafe fn destroy(self, dev: &DeviceData) {
        for s in self.semaphores {
            dev.fns.destroy_semaphore(s, None);
        }
        dev.fns.destroy_image(self.source, None);
        dev.fns.free_memory(self.source_memory, None);
    }
}
