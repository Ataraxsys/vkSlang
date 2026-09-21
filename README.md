I couldn't find any way to use RetroArch's Slang shaders in PC games on Linux, so I built my own with Claude. It works much like vkBasalt (no longer actively developed), but runs .slangp presets directly instead of ReShade shaders, and everything can be tweaked in real time from a GUI (shown below), then saved.

# vkSlang

<img width="3840" height="2160" alt="Capture d&#39;écran_20260919_233503" src="https://github.com/user-attachments/assets/216e1a0a-d980-46d0-aea7-8fc04681205d" />
<img width="3840" height="2160" alt="image" src="https://github.com/user-attachments/assets/56e15952-8087-4dd2-9562-243777d97bdd" />

---

Vulkan layer for Linux that runs **libretro `.slangp` multi-pass presets** (CRT, scanlines, masks, NTSC…) on any Vulkan swapchain through [librashader](https://github.com/SnowflakePowered/librashader). It follows the approach of [vkBasalt](https://github.com/DadSchoorse/vkBasalt), but runs librashader directly instead of ReShade `.fx`.

```sh
ENABLE_VKSLANG=1 VKSLANG_PRESET=~/shaders/crt/crt-royale.slangp VKSLANG_SOURCE_RES=320x240 gamescope -W 3840 -H 2160 -w 320 -h 240 -S integer -F nearest -- %command%
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
cargo build --release --workspace          # -> target/release/libvkslang.so + vkslang-ui
./scripts/install.sh                       # ~/.local/{lib/vkslang,bin,share/vulkan/implicit_layer.d}
```

For Steam/Proton, install **system-wide** as well, because the container imports the layers it finds in `/usr`. Build as your user first, then install without rebuilding, so root never writes into `target/`:

```sh
sudo VKSLANG_NO_BUILD=1 PREFIX=/usr ./scripts/install.sh
```

> A layer installed in `~/.local` takes **priority** over the one in `/usr`. Keeping both means updating both, otherwise a game silently runs the older one. Installing only in `/usr` covers every case.

Without a Rust toolchain, take `libvkslang.so` and `vkslang-ui` from the CI artifact, put them in `target/release/`, and install with `VKSLANG_NO_BUILD=1 ./scripts/install.sh`. The script otherwise always rebuilds, so a stale binary is never installed under a fresh manifest.

Requirements: Rust ≥ 1.95 (for `vkslang-ui`; the layer alone builds with 1.82), a C/C++ compiler (librashader builds SPIRV-Cross and glslang), and the Vulkan loader.

## 4. Configuration

Settings are read from `$VKSLANG_CONFIG`, falling back to `~/.config/vkSlang/vkSlang.conf`. Each key can be overridden by an environment variable `VKSLANG_<KEY>`:

| Variable / key | Example | Purpose |
|---|---|---|
| `ENABLE_VKSLANG` | `1` | Enables the implicit layer (`DISABLE_VKSLANG=1` forces it off). |
| `profile.<executable>` / `profile` | `Amiga 4:3` | Profile loaded automatically for that process, or for everything (file only). |
| `VKSLANG_PRESET` / `preset` | `/…/crt-royale.slangp` | Preset to load, or several separated by commas to chain them. Without one, the layer passes everything through. |
| `VKSLANG_SOURCE_RES` / `source_res` | `320x240`, `/3`, `50%`, `native` | Logical source resolution: fixed, the picture divided by N, or untouched (see below). |
| `VKSLANG_SOURCE_FILTER` / `source_filter` | `nearest` \| `linear` | Filter for the downsample blit. |
| `VKSLANG_SOURCE_RECT` / `source_rect` | `full`, `4:3`, `480,0,2880x2160` | Region of the swapchain image that holds the picture (for gamescope pillarboxing). |
| `VKSLANG_DISPLAY_RECT` / `display_rect` | `full`, `4:3`, `5:4` | Region the preset draws into; stretches the picture when it differs from `source_rect`. |
| `VKSLANG_DISPLAY_SCALE` / `display_scale` | `1.2`, `1.2,1.0` | Scales the drawn area, both axes or each one; below 1 shrinks it, above 1 grows it to the screen then crops what cannot grow. |
| `VKSLANG_PROCESS` / `process` | `gamescope` | Restricts the layer to these executables. |
| `param.<NAME>` | `param.CRT_GAMMA = 2.4` | Overrides preset parameters (file only). |
| `VKSLANG_LOG` | `debug` | Log level. |
| `VKSLANG_LOG_FILE` | `~/vkslang.log` | Also write the log to this file (Steam/Proton, where stderr is out of reach). |
| `VKSLANG_IPC` / `ipc` | `0` | Disables the socket for `vkslang-ui`. |
| `VKSLANG_BRIGHTNESS_NITS` / `brightness_nits` | `200` | HDR reference white (`BrightnessNits`). |
| `VKSLANG_EXPAND_GAMUT` / `expand_gamut` | `0`–`3` | HDR colour boost (`ExpandGamut`): Accurate, Expanded, Wide, Super. |
| `VKSLANG_HDR_OUTPUT` / `hdr_output` | `auto` \| `force` \| `off` | Promote the swapchain to HDR10 (see the HDR section). |
| `VKSLANG_SUBFRAMES` / `subframes` | `1`–`8` | Presentations per application frame (see below). |
| `VKSLANG_SUBFRAME_MODE` / `subframe_mode` | `shader` \| `black` | Run the preset again for each subframe, or insert black frames. |

### Logical resolution (`VKSLANG_SOURCE_RES`)

librashader's `FrameOptions` has no "source resolution" field: the shaders get `OriginalSize`/`SourceSize` from the size of the **input image**. On every present, vkSlang therefore:

1. blits the picture region of the swapchain image (4K, say) into a `source_res` image with a `nearest` filter. That size is either fixed (`320x240`), the picture divided by a factor (`/3`, or `50%`, which keeps the output's aspect ratio whatever the resolution), or `native` for no downscale;
2. passes that image to `FilterChain::frame` as `Original`, with the viewport set to the 4K region.

As a result, scanlines, masks and curvature line up with the 240 original lines rather than the 2160 output lines. With `gamescope -w 320 -h 240 -S integer -F nearest`, the nearest downsample recovers the original pixels exactly.

### Subframes (interlacing, BFI)

CRT presets that simulate interlacing alternate fields on every frame, which at 60 Hz means 30 Hz per field and visible flicker. On a high refresh display, `subframes` makes the layer present the same application frame several times:

```sh
VKSLANG_SUBFRAMES=3   # 60 Hz game on a 240 Hz display -> 180 presentations per second
```

Each subframe advances `FrameCount` and binds `CurrentSubFrame`/`TotalSubFrames`, so presets that alternate fields on `FrameCount` (guest-advanced and friends) interlace at the presentation rate. `subframe_mode = black` inserts black frames instead of running the preset, which costs almost nothing. Both are adjustable live from `vkslang-ui` ("Presentations per frame"), which also shows the measured rates: what the application draws, and what reaches the display. The source line shows the whole chain of sizes: base (swapchain), picture area, input given to the preset, and output.

The layer acquires images of its own for this, one at a time, and never asks the swapchain for extras: raising the image count crashes applications that size their swapchain arrays statically (Qt's QVulkanWindow does). If no image is free within 50 ms, the remaining subframes of that frame are simply skipped. In FIFO the application is naturally limited to `refresh / subframes`, which is why 3 subframes suit a 60 Hz game on a 240 Hz display. Note that a compositor that recomposites (gamescope on its Wayland backend) collapses the subframes; with `gamescope --backend sdl` the layer drives gamescope's own swapchain and they survive.

### Non-square pixels

Old PC and console modes are displayed stretched: 640×360 or 320×200 in memory, shown at 4:3. Reproducing that takes two settings:

- `source_rect` says what to **read**, and `source_res` the size of the input, so the grid stays aligned on the real pixels (640×360).
- `display_rect` says where the preset **draws**. Set to `4:3`, the picture is stretched into that area, **and the shader is stretched with it**: scanlines and mask follow the display geometry, exactly like a CRT fed a 200-line signal.
- `display_scale` resizes the result, per axis in the UI with a lock that keeps the ratio the two axes currently have. Below 1 the drawn area shrinks. Above 1 it grows until it reaches the edges of the screen, and only then is the picture cropped, on the axis that could not grow: a 4:3 area on a 16:9 screen widens first and loses its top and bottom, never its sides. It never overflows the screen, because librashader uses the viewport as its scissor and a scissor reaching outside draws nothing.

### Pixel grid assistant

When you do not know a game's internal resolution, open **Pixel grid…** next to the source settings. The layer grabs the whole image as the application drew it, before the preset, and the window offers two tools:

- **measure**: an adjustable grid. Line it up with the game's pixel blocks and it reads off the pixel size and the resulting resolution, applied as a fixed resolution or a division factor.
- **frame the picture**: a rectangle to drag over the image, inside to move it, near an edge to resize. It sets the picture area (`source_rect`) without typing coordinates, which matters when the game leaves black borders of its own.

The capture is written next to the control socket as raw RGBA (magic `VKSC`, width, height, pixels), downscaled to the requested width, and costs one frame wait only when asked for.

### Steam and Proton

- Install **system-wide** (`PREFIX=/usr sudo -E ./scripts/install.sh`): the Steam container (pressure-vessel) imports the layers it finds in `/usr`.
- Inside the container, `/usr` is the container's own, so **a preset in `/usr/share/libretro` is not visible**. Either keep your presets in your home directory (shared with the container), or let vkSlang find them: a path that does not exist is retried under `/run/host`, where the host filesystem is mounted.
- To see the log of a Steam game, use `VKSLANG_LOG_FILE=$HOME/vkslang.log`.
- The container has its own `$XDG_RUNTIME_DIR`, so the control socket goes to `~/.local/state/vkslang` there, and `vkslang-ui` looks in both directories. `VKSLANG_SOCKET_DIR` overrides the location.
- Games rendering with OpenGL (some ports, even under Proton) are out of reach: the layer only sees Vulkan, including DXVK/VKD3D.

### Gamescope

- `ENABLE_VKSLANG=1` placed before `gamescope` is **inherited by the game**. Set `VKSLANG_PROCESS=gamescope` so only gamescope's output is processed, or leave the variable off to process the game itself.
- vkSlang only hooks **Vulkan swapchains**. Gamescope's nested Wayland backend presents through Wayland subsurfaces, not through a `VkSwapchainKHR`, so use `--backend sdl`, or process the game (`VKSLANG_PROCESS=<game>`) and let gamescope do the scaling.
- If gamescope pillarboxes a 4:3 game on a 16:9 output, set `VKSLANG_SOURCE_RECT=4:3`.

## HDR

vkSlang can give an **SDR game real HDR output**, the way RetroArch does: the layer creates the swapchain in HDR10 while the game keeps rendering 8-bit SDR into it (through a view in its own format), then an HDR-aware preset such as `hdr/crt-sony-megatron-v2-default.slangp` reads those SDR pixels and writes PQ.

```sh
ENABLE_VKSLANG=1 VKSLANG_PRESET=/…/hdr/crt-sony-megatron-v2-default.slangp VKSLANG_SOURCE_RES=640x480 gamescope -W 3840 -H 2160 -f --hdr-enabled -- %command%
```

`hdr_output` (or `VKSLANG_HDR_OUTPUT`) controls this:

| Value | Behaviour |
|---|---|
| `auto` (default) | Promote to HDR10 when the preset writes HDR and the surface supports it. |
| `force` | Promote whenever the surface supports HDR10. |
| `off` | Never touch the swapchain's format. |

The layer enables `VK_EXT_swapchain_colorspace` by itself, so HDR10 formats are visible even when the game never asks for them. `BrightnessNits` (paper white) and `ExpandGamut` are set in the config or live in `vkslang-ui`, which shows the output's and the preset's color spaces.

Remaining limitations:

- **A game that outputs HDR itself** (`DXVK_HDR=1` plus in-game support): its picture reaches the layer already PQ-encoded, while presets expect SDR, so colors will be off. Input conversion is not implemented.
- **An SDR preset on a promoted output** looks wrong: either pick an HDR preset or set `hdr_output = off`. Switching presets live does not un-promote the swapchain, which only happens when the game restarts.
- Without an HDR preset, gamescope's own conversion remains a good option: `--hdr-enabled --hdr-itm-enabled` with any CRT preset in SDR.

## Live control: `vkslang-ui`

```sh
ENABLE_VKSLANG=1 VKSLANG_PRESET=/…/crt-easymode.slangp %command%   # the game
vkslang-ui                                                           # in a separate window
```

The external app (egui, OpenGL, so it never loads the layer itself) connects to the running process and lets you:

- **switch presets** from a searchable browser of the `shaders_slang` folder (compiled in the background, then swapped in with no stutter), or **tick several** to chain them: the passes of the second run on the output of the first, with the order adjustable before applying;
- **adjust parameters** with sliders (min/max/step from `#pragma parameter`, declaration order, section headers, ↺ to restore the preset value);
- change the **source resolution**, the **filter** and the **picture area** live;
- turn the shader **on/off** (bypass);
- **save a profile**: a named look kept in `~/.config/vkSlang/profiles/<name>.json`, holding the preset chain, the parameters you changed, the source resolution, the picture and display areas with their scale, the HDR uniforms and the subframes. One click puts it back on a running game, parameters included (they are applied once the chain has finished compiling). The ☆ next to a profile makes the layer load it **automatically for that process**, by writing `profile.<executable>` into `vkSlang.conf`; anything spelled out in the file or the environment still wins.
- **save** also to a RetroArch-compatible `.slangp` (`#reference` + modified parameters, single preset only) or **as default** in `vkSlang.conf`, leaving your other lines untouched.

### Protocol

One Unix socket per process: `$XDG_RUNTIME_DIR/vkslang/<pid>.sock` (or `~/.local/state/vkslang/` inside a Steam container), one JSON object per line, one response per request (`crates/vkslang-ipc`). Easy to script:

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
