# vkSlang

Vulkan layer for Linux that runs **libretro `.slangp` multi-pass presets** (CRT, scanlines, masks, NTSC…) on any Vulkan swapchain through [librashader](https://github.com/SnowflakePowered/librashader). It follows the approach of [vkBasalt](https://github.com/DadSchoorse/vkBasalt), but runs librashader directly instead of ReShade `.fx`.

```sh
ENABLE_VKSLANG=1 VKSLANG_PRESET=~/shaders/crt/crt-royale.slangp VKSLANG_SOURCE_RES=320x240 \
  gamescope -W 3840 -H 2160 -w 320 -h 240 -S integer -F nearest -- %command%
```

## 1. Rust (`ash` + `librashader`) or C++ (Vulkan SDK + `librashader-capi`)

| Criterion | Rust: `ash` + `librashader` | C++20: Vulkan SDK + `librashader-capi` |
|---|---|---|
| librashader integration | Native. `FilterChain`, `FrameOptions`, `Viewport` are ordinary Rust types, and errors come back as `Result`. | Goes through the C ABI (`libra_vk_filter_chain_create`, …). `librashader.h` loads `librashader.so` at runtime with dlopen, so there are two `.so` files to ship and keep at the same ABI version. |
| Build | One `cargo build`, and librashader is statically linked into `libvkslang.so`. | CMake/Meson plus a Cargo build of the capi, or a system package. |
| Layer plumbing (`vk_layer.h`) | About 100 lines of `#[repr(C)]` structs, because ash does not provide them. | Available as-is (`vk_layer.h`, `vk_layer_dispatch_table.h`). |
| Dispatch tables | `ash::Instance::load_with` and `ash::Device::load_with` take the *next* layer's GIPA/GDPA directly, and librashader accepts those same `ash` objects. | You write the dispatch table by hand (vkBasalt's `vkdispatch.cpp`), then convert it to `libra_device_vk_t`. |
| Memory safety | The unsafe code is limited to the FFI boundary. | Everything is unsafe. |
| Panic / exceptions | `panic = "abort"`, so nothing unwinds into the application. | Exceptions must be disabled or caught at every hook. |

**Recommendation: Rust.** Most of the work is in the librashader integration (the frame contract, the deferred init command buffer, per-frame resources, parameters), and Rust lets you use it with no C ABI and no second library. The one advantage of C++ is `vk_layer.h`, and that is only about a hundred lines to replicate once (`src/loader.rs`). The librashader VK runtime is built on `ash` 0.38, so the `ash::Instance`/`ash::Device` objects the layer builds on the next layer's pointers go straight into `FilterChain::load_from_preset_deferred`.

## 2. Project layout

```
vkSlang/
├── Cargo.toml                  # workspace
├── crates/
│   ├── vkslang/                # cdylib -> libvkslang.so (ash 0.38 + librashader 0.12, runtime-vk)
│   │   └── src/                # lib.rs, loader.rs, state.rs, hooks.rs, render.rs, config.rs,
│   │                           # control.rs (live state), ipc.rs (socket), log.rs
│   ├── vkslang-ipc/            # JSON protocol + client, shared by the layer and the UI
│   └── vkslang-ui/             # egui control panel (main.rs, save.rs)
├── layer/
│   └── vkslang.json            # implicit layer manifest (ENABLE_VKSLANG=1 / DISABLE_VKSLANG=1)
├── config/
│   └── vkSlang.conf            # sample configuration
├── scripts/
│   └── install.sh              # installs .so + manifest into ~/.local (or PREFIX)
└── .github/workflows/ci.yml    # build, test, clippy, exported symbol check
```

## 3. Build and install

```sh
cargo build --release --workspace   # -> target/release/libvkslang.so + vkslang-ui
./scripts/install.sh                # ~/.local/{lib/vkslang,bin,share/vulkan/implicit_layer.d}
PREFIX=/usr sudo -E ./scripts/install.sh   # system-wide
```

Requirements: Rust ≥ 1.80, a C/C++ compiler (librashader builds SPIRV-Cross and glslang), and the Vulkan loader.

## 4. Configuration

Settings are read from `$VKSLANG_CONFIG`, falling back to `~/.config/vkSlang/vkSlang.conf`. Each key can be overridden by an environment variable `VKSLANG_<KEY>`:

| Variable / key | Example | Purpose |
|---|---|---|
| `ENABLE_VKSLANG` | `1` | Enables the implicit layer (`DISABLE_VKSLANG=1` forces it off). |
| `VKSLANG_PRESET` / `preset` | `/…/crt-royale.slangp` | Preset to load. Without one, the layer passes everything through. |
| `VKSLANG_SOURCE_RES` / `source_res` | `320x240` | Logical source resolution (see below). |
| `VKSLANG_SOURCE_FILTER` / `source_filter` | `nearest` \| `linear` | Filter for the downsample blit. |
| `VKSLANG_SOURCE_RECT` / `source_rect` | `full`, `4:3`, `480,0,2880x2160` | Region of the swapchain image that holds the picture (for gamescope pillarboxing). |
| `VKSLANG_PROCESS` / `process` | `gamescope` | Restricts the layer to these executables. |
| `param.<NAME>` | `param.CRT_GAMMA = 2.4` | Overrides preset parameters (file only). |
| `VKSLANG_LOG` | `debug` | Log level. |
| `VKSLANG_IPC` / `ipc` | `0` | Disables the socket for `vkslang-ui`. |
| `VKSLANG_BRIGHTNESS_NITS` / `brightness_nits` | `200` | HDR reference white (`BrightnessNits`). |
| `VKSLANG_EXPAND_GAMUT` / `expand_gamut` | `0`–`3` | HDR colour boost (`ExpandGamut`): Accurate, Expanded, Wide, Super. |

### Logical resolution (`VKSLANG_SOURCE_RES`)

librashader's `FrameOptions` has no "source resolution" field: the shaders get `OriginalSize`/`SourceSize` from the size of the **input image**. On every present, vkSlang therefore:

1. blits the picture region of the swapchain image (4K, say) into a `source_res` image (320×240, say) with a `nearest` filter;
2. passes that image to `FilterChain::frame` as `Original`, with the viewport set to the 4K region.

As a result, scanlines, masks and curvature line up with the 240 original lines rather than the 2160 output lines. With `gamescope -w 320 -h 240 -S integer -F nearest`, the nearest downsample recovers the original pixels exactly.

### Gamescope

- `ENABLE_VKSLANG=1` placed before `gamescope` is **inherited by the game**. Set `VKSLANG_PROCESS=gamescope` so only gamescope's output is processed, or leave the variable off to process the game itself.
- vkSlang only hooks **Vulkan swapchains**. Gamescope's nested Wayland backend presents through Wayland subsurfaces, not through a `VkSwapchainKHR`, so use `--backend sdl`, or process the game (`VKSLANG_PROCESS=<game>`) and let gamescope do the scaling.
- If gamescope pillarboxes a 4:3 game on a 16:9 output, set `VKSLANG_SOURCE_RECT=4:3`.

## HDR (experimental)

On an HDR swapchain (HDR10/PQ or scRGB), vkSlang passes the color space to librashader, which binds `HDRMode`, `BrightnessNits` and `ExpandGamut` for **HDR-aware presets** such as `hdr/crt-sony-megatron-v2-default.slangp`. Brightness and gamut can be set in the config or live in `vkslang-ui`, which also shows the color space of the output and the preset and warns when they don't match.

Limitation: a regular **SDR preset on an HDR output** will look wrong, because there is no inverse tonemapping yet (librashader leaves that to the host).

## Live control: `vkslang-ui`

```sh
ENABLE_VKSLANG=1 VKSLANG_PRESET=/…/crt-easymode.slangp %command%   # the game
vkslang-ui                                                           # in a separate window
```

The external app (egui, OpenGL, so it never loads the layer itself) connects to the running process and lets you:

- **switch presets** from a searchable browser of the `shaders_slang` folder (compiled in the background, then swapped in with no stutter);
- **adjust parameters** with sliders (min/max/step from `#pragma parameter`, declaration order, section headers, ↺ to restore the preset value);
- change the **source resolution**, the **filter** and the **picture area** live;
- turn the shader **on/off** (bypass);
- **save**: a RetroArch-compatible `.slangp` (`#reference` + modified parameters) or **as default** in `vkSlang.conf`, leaving your other lines untouched.

### Protocol

One Unix socket per process: `$XDG_RUNTIME_DIR/vkslang/<pid>.sock`, one JSON object per line, one response per request (`crates/vkslang-ipc`). Easy to script:

```sh
echo '{"cmd":"set_param","name":"MASK_STRENGTH","value":0.5}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/vkslang/<pid>.sock
```

Commands: `get_state`, `set_param`, `reset_params`, `load_preset`, `set_enabled`, `set_source`, `set_hdr`. `VKSLANG_IPC=0` (or `ipc = 0`) disables the socket.

Inside the layer, the IPC thread only writes a desired state (with generation counters). Changes are applied by `vkQueuePresentKHR` on the presenting thread: parameters are uniforms (cost: nothing), source settings rebuild the low-resolution image, and a new preset is compiled on a separate thread with its own command pool.

## 5. How it works

| Hook | Role |
|---|---|
| `vkNegotiateLoaderLayerInterfaceVersion` | Loader interface v2. Hands over the layer's GIPA and GDPA. |
| `vkCreateInstance` / `vkCreateDevice` | Walk `VkLayer*CreateInfo` (`VK_LAYER_LINK_INFO`), advance the chain, and load `ash::Instance`/`ash::Device` on the next layer's pointers. Record `pfnSetDeviceLoaderData`, pick a graphics queue, and enable `VK_KHR_swapchain_mutable_format` when it is available. |
| `vkGetDeviceQueue(2)` | Build a queue → family map, so the layer can submit on the present queue whenever that queue's family allows it. |
| `vkCreateSwapchainKHR` | Add `COLOR_ATTACHMENT \| TRANSFER_SRC` usage (and `MUTABLE_FORMAT` plus a UNORM/sRGB format list for sRGB swapchains). Load the preset with `FilterChain::load_from_preset_deferred`, which compiles shaders now and runs the GPU upload with the first present. Create the source image and the semaphores. |
| `vkQueuePresentKHR` | For each image: `PRESENT_SRC → TRANSFER_SRC`, blit to the source image (`→ SHADER_READ_ONLY`), `TRANSFER_SRC → COLOR_ATTACHMENT`, `filter_chain.frame(...)`, then `COLOR_ATTACHMENT → PRESENT_SRC`. The submit waits on the application's semaphores and signals a per-image semaphore, which the real present then waits on. |
| `vkDestroySwapchainKHR` / `vkDestroyDevice` | Wait on the frame fences, then free everything (the chain is dropped before the device). |

For sRGB swapchains, librashader samples the source and writes the output through **UNORM** views, the same way a RetroArch framebuffer works, so gamma is never applied twice.

## Known limitations

- Queues with `EXCLUSIVE` ownership across families (rendering on a different family than the present queue) are not transferred.
- The fallback graphics queue is not externally synchronized with the application's other threads (vkBasalt has the same limitation).
- There is no x86 (32-bit) build in CI yet. For that, use `cargo build --target i686-unknown-linux-gnu` together with a second manifest.
- The shader cache (`librashader-cache`) is enabled: the first load of a large preset takes several seconds, and later loads are fast.

## License

MPL-2.0 (librashader is MPL-2.0 / GPL-3.0).
