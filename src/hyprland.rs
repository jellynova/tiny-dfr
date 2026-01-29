use std::collections::HashMap;
use std::env;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::Instant;

use cairo::{Context, Format, ImageSurface};
use serde::Deserialize;

/// Information about a Hyprland workspace
#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub id: i32,
    pub name: String,
    pub monitor: String,
    pub is_active: bool,
}

/// Response from Hyprland's j/workspaces command
#[derive(Deserialize)]
struct HyprWorkspace {
    id: i32,
    name: String,
    monitor: String,
}

/// Response from Hyprland's j/activeworkspace command
#[derive(Deserialize)]
struct HyprActiveWorkspace {
    id: i32,
}

/// Client for communicating with Hyprland via IPC
pub struct HyprlandClient {
    socket_path: String,
    available: bool,
}

impl HyprlandClient {
    /// Create a new Hyprland client with a pre-detected socket path
    /// Call detect_socket_path() BEFORE dropping privileges, then pass the result here
    pub fn new(socket_path: Option<String>) -> Self {
        let available = socket_path.is_some();
        Self {
            socket_path: socket_path.unwrap_or_default(),
            available,
        }
    }

    /// Detect the Hyprland IPC socket path
    /// MUST be called before dropping privileges (before PrivDrop)
    /// Returns the socket path if Hyprland is running
    pub fn detect_socket_path() -> Option<String> {
        // Try environment variable first
        if let Ok(instance_sig) = env::var("HYPRLAND_INSTANCE_SIGNATURE") {
            let runtime_dir = env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".to_string());
            let path = format!("{}/hypr/{}/.socket.sock", runtime_dir, instance_sig);
            if std::path::Path::new(&path).exists() {
                return Some(path);
            }
        }

        // Scan /run/user/*/hypr/ for socket files
        Self::find_hyprland_socket()
    }

    /// Scan for Hyprland socket in /run/user/*/hypr/ directories
    fn find_hyprland_socket() -> Option<String> {
        // Check common user IDs (1000 is typical for first user)
        for uid in [1000, 1001, 0] {
            let hypr_dir = format!("/run/user/{}/hypr", uid);
            if let Ok(entries) = std::fs::read_dir(&hypr_dir) {
                for entry in entries.flatten() {
                    let socket_path = entry.path().join(".socket.sock");
                    if socket_path.exists() {
                        return socket_path.to_str().map(|s| s.to_string());
                    }
                }
            }
        }
        None
    }

    /// Check if Hyprland is available
    pub fn is_available(&self) -> bool {
        self.available
    }

    /// Send a command to Hyprland and get the response
    fn send_command(&self, command: &str) -> Option<String> {
        if !self.available {
            return None;
        }

        let mut stream = UnixStream::connect(&self.socket_path).ok()?;
        stream.write_all(command.as_bytes()).ok()?;

        let mut response = String::new();
        stream.read_to_string(&mut response).ok()?;
        Some(response)
    }

    /// Get list of all workspaces
    pub fn get_workspaces(&self) -> Vec<WorkspaceInfo> {
        let Some(response) = self.send_command("j/workspaces") else {
            return Vec::new();
        };

        let workspaces: Vec<HyprWorkspace> = match serde_json::from_str(&response) {
            Ok(w) => w,
            Err(_) => return Vec::new(),
        };

        let active_id = self.get_active_workspace_id().unwrap_or(-1);

        let mut result: Vec<WorkspaceInfo> = workspaces
            .into_iter()
            .map(|w| WorkspaceInfo {
                id: w.id,
                name: w.name,
                monitor: w.monitor,
                is_active: w.id == active_id,
            })
            .collect();

        // Sort by workspace ID
        result.sort_by_key(|w| w.id);
        result
    }

    /// Get the active workspace ID
    fn get_active_workspace_id(&self) -> Option<i32> {
        let response = self.send_command("j/activeworkspace")?;
        let active: HyprActiveWorkspace = serde_json::from_str(&response).ok()?;
        Some(active.id)
    }

    /// Switch to a specific workspace
    pub fn switch_workspace(&self, id: i32) -> bool {
        if !self.available {
            return false;
        }

        // Use dispatch command format
        let command = format!("dispatch workspace {}", id);
        self.send_command(&command).is_some()
    }
}

/// Cache for workspace screenshots
pub struct ScreenshotCache {
    pub screenshots: HashMap<i32, ImageSurface>,
    last_update: Instant,
    refresh_interval_ms: u64,
    screenshot_command: Option<String>,
}

impl ScreenshotCache {
    /// Create a new screenshot cache
    pub fn new(refresh_interval_ms: u64, screenshot_command: Option<String>) -> Self {
        Self {
            screenshots: HashMap::new(),
            last_update: Instant::now(),
            refresh_interval_ms,
            screenshot_command,
        }
    }

    /// Check if the cache needs refreshing
    pub fn needs_refresh(&self) -> bool {
        self.last_update.elapsed().as_millis() as u64 > self.refresh_interval_ms
    }

    /// Capture a screenshot of the specified monitor and scale it to target height
    pub fn capture_screenshot(&mut self, monitor: &str, target_height: i32) -> Option<ImageSurface> {
        let png_data = self.capture_raw(monitor)?;
        self.scale_screenshot(&png_data, target_height)
    }

    /// Capture raw PNG data from grim
    fn capture_raw(&self, monitor: &str) -> Option<Vec<u8>> {
        let args = if let Some(ref cmd) = self.screenshot_command {
            // Custom command - replace {} with monitor name
            let cmd_with_monitor = cmd.replace("{}", monitor);
            vec!["-c".to_string(), cmd_with_monitor]
        } else {
            // Default grim command
            vec!["-c".to_string(), format!("grim -o {} -", monitor)]
        };

        // Find Wayland display socket for grim
        let wayland_display = Self::find_wayland_display();

        let mut cmd = Command::new("sh");
        cmd.args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        // Set environment for Wayland access
        if let Some(display) = &wayland_display {
            cmd.env("WAYLAND_DISPLAY", display);
        }
        cmd.env("XDG_RUNTIME_DIR", "/run/user/1000");

        let output = cmd.output().ok()?;

        if output.status.success() && !output.stdout.is_empty() {
            Some(output.stdout)
        } else {
            None
        }
    }

    /// Find Wayland display socket
    fn find_wayland_display() -> Option<String> {
        // Check environment first
        if let Ok(display) = env::var("WAYLAND_DISPLAY") {
            return Some(display);
        }

        // Look for wayland-* socket in /run/user/1000/
        let runtime_dir = "/run/user/1000";
        if let Ok(entries) = std::fs::read_dir(runtime_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with("wayland-") && !name_str.ends_with(".lock") {
                    return Some(name_str.to_string());
                }
            }
        }
        None
    }

    /// Scale a PNG screenshot to the target height while maintaining aspect ratio
    fn scale_screenshot(&self, png_data: &[u8], target_height: i32) -> Option<ImageSurface> {
        // Create a surface from the PNG data
        let mut cursor = std::io::Cursor::new(png_data);
        let source_surface = ImageSurface::create_from_png(&mut cursor).ok()?;

        let src_width = source_surface.width();
        let src_height = source_surface.height();

        if src_height == 0 || src_width == 0 {
            return None;
        }

        // Calculate scaled dimensions
        let scale = target_height as f64 / src_height as f64;
        let target_width = (src_width as f64 * scale).round() as i32;

        // Create target surface
        let target_surface = ImageSurface::create(Format::ARgb32, target_width, target_height).ok()?;
        let context = Context::new(&target_surface).ok()?;

        // Scale and draw
        context.scale(scale, scale);
        context.set_source_surface(&source_surface, 0.0, 0.0).ok()?;
        context.paint().ok()?;

        Some(target_surface)
    }

    /// Refresh screenshot for the active workspace only
    /// (We can only capture what's currently visible on screen)
    pub fn refresh(&mut self, workspaces: &[WorkspaceInfo], target_height: i32) {
        // Only capture the active workspace - that's the only one we can actually see
        for workspace in workspaces {
            if workspace.is_active {
                if let Some(surface) = self.capture_screenshot(&workspace.monitor, target_height) {
                    self.screenshots.insert(workspace.id, surface);
                }
                break;
            }
        }

        self.last_update = Instant::now();
    }

    /// Get a cached screenshot for a workspace
    pub fn get(&self, workspace_id: i32) -> Option<&ImageSurface> {
        self.screenshots.get(&workspace_id)
    }

}
