# Changelog

## [1.0.0] - 2026-10-02

First stable release. vkSlang runs libretro `.slangp` presets on any Vulkan
swapchain through librashader, with a live control panel.

### Layer

- Implicit Vulkan layer (`ENABLE_VKSLANG=1`), linked with `-Bsymbolic` so it
  works in applications that link libvulkan directly.
- Logical source resolution: native, divided by a factor (`/3`), or fixed
  (`320x240`), so scanlines and masks follow the game's lines on a 4K output.
- Picture area and display area (`4:3`, `X,Y,WxH`), with separate scales for
  the drawn area and for the picture inside it, per axis.
- Exact pixel duplication per axis for non-square modes (DOS 320×200 gives the
  preset 400 real lines).
- Several presets chained into one, each switchable on its own without
  resetting the others' parameters.
- Subframes: each application frame presented several times so interlacing
  presets alternate fields at the display rate, or black frame insertion.
- HDR: an SDR game's swapchain promoted to HDR10 for HDR presets (Sony
  Megatron), with `BrightnessNits` and `ExpandGamut`.
- Named profiles, and a default profile per executable.
- Steam/Proton: paths retried under `/run/host`, control socket reachable from
  the container, `VKSLANG_LOG_FILE`.

### Control panel (`vkslang-ui`)

- Preset browser as a folder tree with search, chain editing.
- Live parameters, source and area settings, HDR, subframes, measured rates.
- Pixel grid assistant: measure a game's resolution and frame its picture on a
  capture.
- Divisor calculator: the source divisor from the screen and game resolutions,
  integer-scaled or fitted, and "frame the game", which sets the picture and
  display areas to exactly where gamescope draws the game, so a 4:3 game is
  processed on its own pixels only.
- Profiles: save, apply, star as a per-process default, delete (with
  confirmation); export a RetroArch-compatible `.slangp`.

### Fixed since the last development builds

- Square tiles of an older frame under gamescope `--backend sdl`: gamescope
  presents from its compute queue with no wait semaphore, and the layer now
  orders its work after that queue. Swapchain images are shared between every
  queue family in play.
- GPU memory leaked on every source settings change with pixel duplication on.
- An image acquired for a subframe could be kept forever when nothing could
  be rendered into it; subframes are now presented on the application's queue.
- Applying a profile kept the previous profile's parameter tweaks.
- Each "Save as default" added one more marker line to `vkSlang.conf`.
- Pixel grid captures and sockets of exited processes were never deleted.
- The control panel could grow wider than its window and clip its right edge;
  the save buttons could be pushed out of sight.
- `process = ...` is now case-insensitive, like `profile.<executable>`.

### Changed

- Control protocol v15 (`subframes_max` removed): restart running games after
  updating, the panel says so otherwise.
- The sample `vkSlang.conf` no longer sets anything: whatever it sets applies to
  every process, gamescope included.
- "Save as default" is now "Save for all games", and says where it applies.
- `install.sh --uninstall`; both directions warn when another copy takes
  priority.
- MPL-2.0 license file added.
