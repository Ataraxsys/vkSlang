I couldn't find any way to use RetroArch's Slang shaders in PC games on Linux, so I built my own with Claude. It works much like vkBasalt (no longer actively developed), but runs .slangp presets directly instead of ReShade shaders, and everything can be tweaked in real time from a GUI (shown below), then saved.

# vkSlang

<img width="3840" height="2160" alt="Capture d&#39;écran_20260919_233503" src="https://github.com/user-attachments/assets/216e1a0a-d980-46d0-aea7-8fc04681205d" />
<img width="3840" height="2160" alt="image" src="https://github.com/user-attachments/assets/56e15952-8087-4dd2-9562-243777d97bdd" />

---

Vulkan layer for Linux that runs **libretro `.slangp` multi-pass presets** (CRT, scanlines, masks, NTSC…) on any Vulkan swapchain through [librashader](https://github.com/SnowflakePowered/librashader), with **`vkslang-ui`**, a control panel to change presets and parameters while the game runs.

- Works with native Vulkan games, Proton/DXVK/VKD3D, and through gamescope with anything else (OpenGL, SDL, Wine GDI…).
- Logical source resolution, so scanlines follow the game's 240 lines rather than the 2160 of a 4K screen.
- Picture and display areas, non-square pixels, exact pixel duplication.
- Several presets chained, subframes for interlacing and black frame insertion, real HDR output for SDR games.
- Named profiles, loaded automatically per game.

## Contents

1. [Install](#install)
2. [First run](#first-run)
3. [Recipes](#recipes): native games, Steam and Proton, gamescope, non-Vulkan programs
4. [The control panel](#the-control-panel-vkslang-ui)
5. [Configuration](#configuration)
6. [Concepts](#concepts): source resolution, areas and scales, non-square pixels, subframes, HDR
7. [Troubleshooting](#troubleshooting)
8. [How it works](#how-it-works)
9. [Limitations](#limitations)
10. [Building and development](#building-and-development)

## Install

### From a release

Download `vkslang-<version>-x86_64-linux.tar.gz` from the [releases](https://github.com/Ataraxsys/vkSlang/releases), then:

```sh
tar xf vkslang-*-x86_64-linux.tar.gz
cd vkslang-*-x86_64-linux
VKSLANG_NO_BUILD=1 ./scripts/install.sh          # into ~/.local
```

The release binaries are built on Ubuntu 24.04 and need glibc 2.39 or newer. On an older system, build from source.

### From source

Requirements: Rust 1.95 or newer, a C/C++ compiler (librashader builds SPIRV-Cross and glslang), and the Vulkan loader.

```sh
git clone https://github.com/Ataraxsys/vkSlang && cd vkSlang
./scripts/install.sh                              # builds, then installs into ~/.local
```

This installs `~/.local/lib/vkslang/libvkslang.so`, the layer manifest in `~/.local/share/vulkan/implicit_layer.d/`, `~/.local/bin/vkslang-ui`, and a sample `~/.config/vkSlang/vkSlang.conf` (only if you have none, and it sets nothing).

### System-wide (Steam)

The Steam container (pressure-vessel) only imports the layers it finds in `/usr`. Build as your user, then install without rebuilding so root never writes into `target/`:

```sh
./scripts/install.sh                              # or take the binaries from a release
sudo VKSLANG_NO_BUILD=1 PREFIX=/usr ./scripts/install.sh
```

> A copy in `~/.local` **takes priority** over the one in `/usr`. Keep only one, or update both: otherwise a game silently runs the older one. The script warns when it sees both.

### Uninstall

```sh
./scripts/install.sh --uninstall                  # ~/.local
sudo PREFIX=/usr ./scripts/install.sh --uninstall # /usr
```

Your configuration and profiles in `~/.config/vkSlang` are left alone.

## First run

The layer is off unless `ENABLE_VKSLANG=1` is set, and does nothing until it has a preset. Try it on `vkcube`:

```sh
ENABLE_VKSLANG=1 VKSLANG_PRESET=/usr/share/libretro/shaders/shaders_slang/crt/crt-easymode.slangp vkcube &
vkslang-ui
```

The panel connects to `vkcube` on its own. Pick another preset in the tree, move the sliders, set the source resolution: everything applies on the next frame. Presets come from the `shaders_slang` collection (`libretro-shaders-slang` on Arch, or RetroArch's own download in `~/.config/retroarch/shaders`).

## Recipes

### A native Vulkan game

```sh
ENABLE_VKSLANG=1 VKSLANG_PRESET=/path/to/crt-royale.slangp %command%
```

as Steam launch options, or the same in front of the command in a terminal or launcher. Then open `vkslang-ui`, tune, and save a **profile** with the ☆ set: next time the game picks it up by itself, and `VKSLANG_PRESET` can go.

### Steam and Proton (DXVK, VKD3D)

The same launch options work for Windows games, as long as the layer is installed **in `/usr`** (see [System-wide](#system-wide-steam)). Inside the container:

- `/usr` is the container's own. A path such as `/usr/share/libretro/...` is retried under `/run/host`, where the host is mounted, so it keeps working. Presets in your home directory work as they are.
- `$XDG_RUNTIME_DIR` is private, so the control socket goes to `~/.local/state/vkslang`; `vkslang-ui` looks there too.
- To read the log: `VKSLANG_LOG_FILE=$HOME/vkslang.log`.

32-bit games are out of reach of the layer itself (it is built for x86-64 only): go through gamescope, below.

### Through gamescope

Running the shader on **gamescope's output** works for any game, 32-bit, OpenGL or not, and lets gamescope do the integer scaling first:

```sh
ENABLE_VKSLANG=1 VKSLANG_PROCESS=gamescope gamescope --backend sdl -f -W 3840 -H 2160 -w 640 -h 480 -S integer -F nearest -- %command%
```

- **`--backend sdl` is required.** Only then does gamescope present through a Vulkan swapchain of its own; its default Wayland backend has none, and the layer would have nothing to process.
- **`VKSLANG_PROCESS=gamescope`** keeps the layer out of the game: `ENABLE_VKSLANG=1` is inherited by every child process.
- The preset comes from a profile starred for `gamescope`, from `preset =` in `vkSlang.conf`, or from `VKSLANG_PRESET`.
- With integer scaling, set the source resolution to **divide** and use **Find the divisor** in the panel: give it the game's resolution and it computes how many screen pixels each game pixel covers (640×480 on a 4K screen: ÷4).
- **frame the game**, next to it, goes further: the preset reads and draws only where gamescope put the game (2560×1920 at 640,120 for that example), so a 4:3 game keeps its black bars, borders and bezels stay around the game rather than the whole screen, and the preset's grid sits exactly on the game's pixels. The `4:3` buttons of the areas only approximate this: they take the largest 4:3 region of the screen, black bars included.

### Programs that do not use Vulkan

OpenGL games, SDL programs and Wine applications drawing with GDI (AppleWin, for instance) never create a Vulkan swapchain. Put gamescope in front, exactly as above, and the shader runs on its output:

```sh
ENABLE_VKSLANG=1 VKSLANG_PROCESS=gamescope gamescope --backend sdl -f -w 560 -h 384 -S integer -F nearest -- wine AppleWin.exe
```

## The control panel: `vkslang-ui`

An ordinary window (egui on OpenGL, so it never loads the layer itself) that finds running games by itself. It speaks French and English (FR/EN at the top right, French by default on a French system).

### Set up this game

**🎯 Set up this game** walks through a new game in six steps, every change applied to the game as you go:

1. **Capture**: a full-resolution picture of the game as it draws it, before the shader.
2. **Game zone**: where the game is on the screen. *Detect* removes the black bars; *Adjust by hand* drags the frame when the game has black borders of its own.
3. **Pixels**: how many screen pixels one game pixel covers, per axis. *Detect* measures it on the capture, including pixels taller than wide, and lines the zone up on the grid; *Check with the grid* overlays it at ×4 so every cell holds one block of colour. Typing the game's resolution works too.
4. **Shape and size**: *4:3 monitor of the time* (320×200 comes out with pixels 1.2 times taller, as on a VGA screen), *Square pixels* (duplicating lines makes the picture taller), or *As shown now*; *Whole multiple* gives every scanline the same thickness, *Fill the screen*, or *Where it is*. **Duplicate** repeats each game pixel: the shader works on the repeated lines. A miniature screen shows the result.
5. **Shader**: search and click a preset.
6. **Profile**: save it all under a name, loaded automatically next time this program starts.

### Tabs

- **Picture**: the same four questions at any time (capture, zone, pixels, shape and size, with the preview and the capture), and the raw settings under *Advanced settings*: source resolution with the divisor calculator, picture and display areas, scales, duplication, filter.
- **Shader**: the preset tree (a plain click runs one, ticking several **chains** them, each entry switchable on its own, parameter tweaks kept when the chain changes) and the parameters, in the preset's order with ↺ to restore a value.
- **Display**: HDR paper white and gamut, presentations per frame (subframes) with the measured rates.
- **Profiles**: profiles (☆ loads one automatically for that executable, by writing `profile.<executable>` into `vkSlang.conf`), export to a RetroArch-compatible `.slangp`, or *Save for all games* into `vkSlang.conf`, which applies to every process the layer runs in.

Programs that load the layer but show nothing (gamescope without `--backend sdl`, launchers) are hidden behind "+N idle".

## Configuration

Settings are read from `$VKSLANG_CONFIG`, else `$XDG_CONFIG_HOME/vkSlang/vkSlang.conf` (`~/.config/vkSlang/vkSlang.conf`). Every key can be overridden by an environment variable `VKSLANG_<KEY>`, which is the easiest way to give one game its own settings from a launcher. Precedence, strongest first: environment, `vkSlang.conf`, the profile for the executable, defaults.

| Key / variable | Example | Purpose |
|---|---|---|
| `ENABLE_VKSLANG` | `1` | Enables the layer (`DISABLE_VKSLANG=1` forces it off). |
| `profile.<executable>`, `profile` | `Amiga 4:3` | Profile loaded for that executable (case-insensitive), or for every process. File only. |
| `preset` | `/…/crt-royale.slangp` | Preset, or several separated by commas to chain them. Without one the layer stays out of the way. |
| `param.<NAME>` | `param.CRT_GAMMA = 2.4` | Preset parameter override. File only. |
| `process` | `gamescope` | Executables the layer runs in (comma separated, case-insensitive). Empty: all. |
| `source_res` | `native`, `/3`, `50%`, `320x240` | Logical source resolution. |
| `source_filter` | `nearest`, `linear` | Filter of the reduction to that resolution. |
| `pixel_duplicate` | `2`, `1,2` | Repeat each source pixel per axis. |
| `source_rect` | `full`, `4:3`, `480,0,2880x2160` | Region of the image holding the picture. |
| `display_rect` | `full`, `4:3`, `5:4` | Region the preset draws into. |
| `display_scale` | `1.2`, `1.2,1.0` | Scales the drawn area (preset and picture together). |
| `source_scale` | `1.2`, `1,2` | Scales the picture inside it (the preset keeps its geometry). |
| `subframes` | `1`–`8` | Presentations per application frame. |
| `subframe_mode` | `shader`, `black` | Run the preset again for each subframe, or insert black frames. |
| `hdr_output` | `off`, `auto`, `on` | HDR10 output (see HDR). |
| `hdr_peak_nits` | `1000` | HDR peak luminance, for the conversion. |
| `hdr_contrast` | `1.0` | HDR contrast, for the conversion. |
| `brightness_nits` | `200` | HDR paper white (`BrightnessNits`). |
| `expand_gamut` | `0`–`3` | HDR gamut (`ExpandGamut`): Accurate, Expanded, Wide, Super. |
| `ipc` | `0` | Disables the control socket. |
| `VKSLANG_LOG` | `debug` | Log level: `error`, `warn`, `info` (default), `debug`. |
| `VKSLANG_LOG_FILE` | `~/vkslang.log` | Also write the log to this file. |
| `VKSLANG_SOCKET_DIR` | `/tmp/vkslang` | Where control sockets go. |
| `VKSLANG_CONFIG` | `~/vkslang-test.conf` | Another configuration file. |

`config/vkSlang.conf` documents every key in place.

## Concepts

### Source resolution

The shaders get `OriginalSize` and `SourceSize` from the size of their input image, and librashader has no other way to tell them the game's resolution. So on every frame vkSlang reduces the picture to the **source resolution** before handing it over, and the preset draws back at full size:

1. the picture region of the swapchain image (4K, say) is copied into a `source_res` image: fixed (`320x240`), the picture divided by a factor (`/3`, or `50%`, which keeps the aspect ratio at any output resolution), or `native` for no reduction;
2. that image is the preset's `Original`, and the viewport is the full-size display area.

Scanlines, masks and curvature then line up with the game's 240 lines rather than the 2160 of the screen. With gamescope's integer scaling and a nearest filter, the reduction recovers the original pixels exactly; **Find the divisor** gives the factor.

### Areas and scales

- **`source_rect`** is what is read: the picture, without gamescope's black bars (`4:3` on a 16:9 screen) or a game's own borders (frame it with the pixel grid's rectangle).
- **`display_rect`** is where the preset draws. Different from `source_rect`, it stretches the picture, **and the shader with it**: scanlines and mask follow the display geometry, like a CRT fed that signal.
- **`display_scale`** resizes the drawn area: preset and picture grow **together**, never past the screen.
- **`source_scale`** resizes the picture **inside** that area by reading a smaller region. The preset gets the same number of pixels into the same area, so scanlines and mask keep their size: `1,2` makes the picture twice as tall without touching the preset.

### Non-square pixels

Old PC and console modes were displayed stretched: 640×360 or 320×200 in memory, shown at 4:3.

- Set `source_res` to the real size (640×360) so the grid stays on the real pixels, and `display_rect = 4:3` to stretch it.
- **`pixel_duplicate`** repeats each pixel. A DOS 320×200 mode with `1,2` hands the preset 400 real lines rather than 200 stretched ones, so scanlines work on actual lines. The picture is reduced to the real grid first, then copied with a nearest blit between exact multiples: every pixel really is duplicated, not resampled.

### Subframes

CRT presets that simulate interlacing alternate fields on every frame, which at 60 Hz means visible 30 Hz flicker. On a high refresh display, `subframes` presents each application frame several times:

```sh
VKSLANG_SUBFRAMES=3   # 60 Hz game on a 240 Hz display: 180 presentations per second
```

Each subframe advances `FrameCount` and binds `CurrentSubFrame`/`TotalSubFrames`, so presets that alternate fields interlace at the presentation rate. `subframe_mode = black` inserts black frames instead (BFI), which costs almost nothing. The panel shows both rates.

The layer acquires its own images one at a time and never asks for extra ones (that crashes applications that size their swapchain arrays statically, such as Qt's). In FIFO the application is limited to `refresh / subframes`. A compositor that recomposites collapses subframes: through gamescope, use `--backend sdl`.

### HDR

vkSlang gives games **real HDR output the way RetroArch does**: the swapchain is created in HDR10 while the game keeps rendering 8-bit SDR into it, through a view in its own format, and the picture is turned into HDR at the end of the chain.

| `hdr_output` (panel: *Display › HDR*) | Behaviour |
|---|---|
| `off` | Never touch the swapchain's format. |
| `auto` (default) | HDR10 only for presets that write HDR themselves, such as `hdr/crt-sony-megatron-v2-default.slangp`. |
| `on` | Always HDR10. **Any preset** gets a final conversion pass, as RetroArch's HDR option: linearised with the contrast as gamma, inverse tone mapped so mid grey lands on paper white and white on the display's peak, moved to BT.2020 (expanded or not), encoded PQ. |

The settings are RetroArch's: **peak luminance** (`hdr_peak_nits`, what the display can show), **paper white** (`brightness_nits`, the mid tones), **contrast** (`hdr_contrast`) and **gamut** (`expand_gamut`, Accurate to Super). HDR presets receive paper white and gamut as `BrightnessNits` and `ExpandGamut`.

HDR can be switched on and off **while the game runs**: the layer asks the game to recreate its swapchain (`VK_SUBOPTIMAL_KHR`), which most games do at once; otherwise restart it. The display must have HDR enabled (KDE: *Display configuration › HDR*); gamescope is not needed. The layer enables `VK_EXT_swapchain_colorspace` itself, so the HDR10 formats are visible even to games that never ask for them.

- A game that outputs HDR itself (`DXVK_HDR=1`) hands the preset PQ-encoded pixels while presets expect SDR: leave HDR off for it.
- Through gamescope, `--hdr-enabled --hdr-itm-enabled` is another route, with any SDR preset.

## Troubleshooting

**`vkslang-ui` lists nothing.** The layer only starts its control socket in a process that has a preset at startup: set one in the launch options, in `vkSlang.conf`, or star a profile for that executable. Also check that the layer loads at all:

```sh
ENABLE_VKSLANG=1 VK_LOADER_DEBUG=layer vkcube 2>&1 | grep -i vkslang
```

**The process shows as idle ("no swapchain").** It loaded the layer but presents nothing the layer can process: gamescope without `--backend sdl`, or a launcher. With gamescope, add `--backend sdl` and `VKSLANG_PROCESS=gamescope`.

**The shader applies under gamescope but not to the game you meant**, or to everything. `ENABLE_VKSLANG=1` is inherited by every child: use `VKSLANG_PROCESS`, and keep looks out of the global `vkSlang.conf` (prefer starred profiles).

**An update changed nothing.** Two copies are installed and `~/.local` wins over `/usr`. `VK_LOADER_DEBUG=layer` shows which library is loaded; remove one with `install.sh --uninstall`.

**The panel says the layer speaks another protocol.** The game still runs the previous version of the layer: restart it.

**Steam game, no effect.** Install in `/usr`, keep presets in your home directory or under `/usr` (retried under `/run/host`), and read `VKSLANG_LOG_FILE`.

## How it works

| Hook | Role |
|---|---|
| `vkNegotiateLoaderLayerInterfaceVersion` | Loader interface v2; hands over the layer's GIPA and GDPA. |
| `vkCreateInstance` / `vkCreateDevice` | Walk the loader's link info, call down, load `ash` tables on the next layer's pointers. Enable `VK_EXT_swapchain_colorspace` and `VK_KHR_swapchain_mutable_format` when useful, pick a graphics queue for the layer. |
| `vkGetDeviceQueue(2)` | Map each queue to its family. |
| `vkCreateSwapchainKHR` | Add `COLOR_ATTACHMENT \| TRANSFER_SRC` usage, a UNORM view format for sRGB swapchains, HDR10 promotion, and share the images between every queue family in play. Load the preset (`FilterChain::load_from_preset_deferred`) and create the source images. |
| `vkQueuePresentKHR` | Order the layer's work after the application's queue, copy the picture into the source image, run the chain into the swapchain image, repaint the bars opaque black, then present waiting on the layer's semaphore. Subframes follow. |
| `vkDestroySwapchainKHR` / `vkDestroyDevice` | Wait for the frames in flight and free everything, the chain before the device. |

Applications may present from a queue the layer cannot render on: gamescope composites with a compute shader and presents from its compute queue, **with no wait semaphore**, relying on that queue's order. The layer therefore submits an empty batch on the application's queue that signals a semaphore (a signal covers everything submitted before it on that queue) and waits on it, and creates the swapchain images `CONCURRENT` across the families so no ownership transfer is needed. Without this, the picture was read while gamescope was still writing it, and showed square tiles of an older frame.

The control socket is served by a thread that only writes a desired state with generation counters; everything is applied by `vkQueuePresentKHR` on the presenting thread. Parameters are uniforms, source settings rebuild the source images, and a new preset compiles on a separate thread with its own command pool, then swaps in.

### Control protocol

One Unix socket per process, `$XDG_RUNTIME_DIR/vkslang/<pid>.sock` (`~/.local/state/vkslang/` inside a Steam container), one JSON object per line, one response per request with the full state. Easy to script:

```sh
echo '{"cmd":"set_param","name":"MASK_STRENGTH","value":0.5}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/vkslang/<pid>.sock
```

Commands: `get_state`, `set_param`, `reset_params`, `load_presets`, `set_enabled`, `set_source`, `set_hdr`, `set_subframes`, `capture`. See `crates/vkslang-ipc` for the exact messages. The protocol has a version number; the panel refuses a layer that speaks another one and says so.

## Limitations

- **x86-64 only.** 32-bit Vulkan games (DXVK for 32-bit Windows games) cannot load the layer; run them through gamescope.
- **Vulkan only.** OpenGL and GDI programs need gamescope in front.
- The layer's graphics queue is not externally synchronized with the application's other threads when the application presents from another queue family (vkBasalt has the same limitation).
- No conversion from an HDR game's output to the SDR input presets expect.
- The shader cache (`librashader-cache`) is on: the first load of a large preset takes a few seconds, later ones are fast.

## Building and development

```sh
cargo build --release --workspace     # target/release/libvkslang.so and vkslang-ui
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

```
crates/vkslang/       the layer (cdylib): hooks, render path, configuration, control socket
crates/vkslang-ipc/   protocol, client and profiles, shared by the layer and the panel
crates/vkslang-ui/    the control panel (egui)
layer/vkslang.json    implicit layer manifest
config/vkSlang.conf   documented sample configuration
scripts/install.sh    install and uninstall
```

The layer is written in Rust with `ash` and librashader: librashader's Vulkan runtime is built on `ash` 0.38, so the `ash::Instance` and `ash::Device` the layer loads on the next layer's pointers go straight into the filter chain, with no C ABI and no second library to ship. The library is linked with `-Bsymbolic` and every pointer handed to the loader targets a private function, so the layer also works in applications that link libvulkan directly. Releases are published by pushing a `v*` tag matching the crate version (see `CHANGELOG.md`).

## License

MPL-2.0 (librashader is MPL-2.0 / GPL-3.0).
