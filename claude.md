# Claude Instructions for tiny-dfr

This file contains instructions for Claude Code when working on this project.

## Building and Installing

**IMPORTANT:**
- ALWAYS run `cargo build --release` yourself FIRST and fix any errors/warnings
- THEN provide the install commands to the user
- ALWAYS provide ALL commands as ONE copy-pasteable block
- ALWAYS use FULL PATHS in all commands (e.g., `/home/nova/git/tiny-dfr/target/release/tiny-dfr`, not `target/release/tiny-dfr`)

After building successfully, provide this install command block:

```bash
cd /home/nova/git/tiny-dfr && \
cargo build --release && \
sudo systemctl stop tiny-dfr && \
sudo cp /home/nova/git/tiny-dfr/target/release/tiny-dfr /usr/bin/tiny-dfr && \
sudo systemctl start tiny-dfr && \
sudo systemctl status tiny-dfr
```

**To view logs:**
```bash
sudo journalctl -u tiny-dfr -f
```

## Project Structure

- `src/main.rs` - Main application logic, touch handling, rendering
- `src/config.rs` - Configuration loading and layer management
- `src/display.rs` - DRM display backend
- `src/backlight.rs` - Backlight management
- `src/fonts.rs` - Font configuration
- `src/pixel_shift.rs` - Pixel shift to prevent burn-in

## Configuration

- System config: `/usr/share/tiny-dfr/config.toml`
- User config: `/etc/tiny-dfr/config.toml`
- Icons: `/usr/share/tiny-dfr/` and `/etc/tiny-dfr/`

## Current Branch: next-iteration

This branch includes:
- Multi-layer support with Fn key cycling
- Interactive slider controls (Volume, Brightness, KeyboardBacklight)
- Touch gesture detection (tap vs drag)
- Configurable colors with per-button overrides
- Slider overlay with visual feedback

## Development Notes

- The slider implementation uses:
  - Fixed 300px width for consistent UX
  - Constrained interaction area matching visual bounds
  - **WirePlumber wpctl** for PipeWire volume control (decimal 0.0-1.0 format)
  - Direct sysfs writes for brightness/keyboard backlight
  - Custom commands support via `slider_get_command` and `slider_set_command`

- **Privilege drop**: tiny-dfr drops privileges to `nobody` user with groups: `input`, `video`
  - `input`: Required for touch input device access
  - `video`: Required for DRM display access

- **Volume control**: Uses VolumeUp/VolumeDown key events (reliable, bypasses session isolation)
  - Tries to read current volume with wpctl for display (falls back to 50% if unavailable)
  - Calculates delta between current and target volume
  - Sends appropriate number of key events (~5% per key press)
  - Works reliably without session access issues

- When testing sliders:
  - Short tap = view current value
  - Drag horizontally = adjust value
  - Overlay auto-dismisses after 2 seconds
  - Fn key dismisses active slider

## Always Remember

When the user asks you to make changes and test them:
1. Make the changes
2. Run `cargo build --release`
3. Provide the installation commands above
4. The user will handle actually running the installation commands
