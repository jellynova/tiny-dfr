use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

pub struct AudioVisualizer {
    pub bars: Vec<f32>,
    bar_count: usize,
    smoothing: f32,
    capture_thread: Option<thread::JoinHandle<()>>,
    shared_samples: Arc<Mutex<Vec<f32>>>,
    running: Arc<Mutex<bool>>,
    child_process: Arc<Mutex<Option<Child>>>,
}

impl AudioVisualizer {
    pub fn new(bar_count: usize, smoothing: f32) -> Self {
        let bar_count = bar_count.max(1);
        Self {
            bars: vec![0.0; bar_count],
            bar_count,
            smoothing: smoothing.clamp(0.0, 1.0),
            capture_thread: None,
            shared_samples: Arc::new(Mutex::new(Vec::new())),
            running: Arc::new(Mutex::new(false)),
            child_process: Arc::new(Mutex::new(None)),
        }
    }

    pub fn start(&mut self) -> Result<(), String> {
        if self.is_running() { return Ok(()); }

        let child = Command::new("pw-record")
            .args(["--target=@DEFAULT_AUDIO_SINK@.monitor", "-r", "44100", "--format=f32", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("Failed to spawn pw-record: {}", e))?;

        *self.child_process.lock().unwrap() = Some(child);
        *self.running.lock().unwrap() = true;

        let shared = Arc::clone(&self.shared_samples);
        let running = Arc::clone(&self.running);
        let child_proc = Arc::clone(&self.child_process);

        self.capture_thread = Some(thread::spawn(move || {
            capture_loop(shared, running, child_proc);
        }));
        Ok(())
    }

    pub fn stop(&mut self) {
        *self.running.lock().unwrap() = false;
        if let Some(ref mut child) = *self.child_process.lock().unwrap() {
            let _ = child.kill();
            let _ = child.wait();
        }
        *self.child_process.lock().unwrap() = None;
        if let Some(h) = self.capture_thread.take() { let _ = h.join(); }
        self.bars.iter_mut().for_each(|b| *b = 0.0);
    }

    pub fn update(&mut self) {
        let samples = {
            let mut shared = self.shared_samples.lock().unwrap();
            let s = shared.clone();
            shared.clear();
            s
        };

        if samples.is_empty() {
            for bar in &mut self.bars {
                *bar *= self.smoothing;
                if *bar < 0.001 { *bar = 0.0; }
            }
            return;
        }

        let spectrum = simple_spectrum(&samples, self.bar_count);
        for (i, &target) in spectrum.iter().enumerate() {
            let current = self.bars[i];
            self.bars[i] = if target > current {
                current + (target - current) * (1.0 - self.smoothing * 0.5)
            } else {
                current + (target - current) * (1.0 - self.smoothing)
            };
        }
    }

    pub fn is_running(&self) -> bool { *self.running.lock().unwrap() }
    pub fn bar_count(&self) -> usize { self.bar_count }
}

impl Drop for AudioVisualizer {
    fn drop(&mut self) { self.stop(); }
}

fn capture_loop(shared: Arc<Mutex<Vec<f32>>>, running: Arc<Mutex<bool>>, child: Arc<Mutex<Option<Child>>>) {
    let mut buf = [0u8; 4096];
    loop {
        if !*running.lock().unwrap() { break; }

        let n = {
            let mut lock = child.lock().unwrap();
            match lock.as_mut().and_then(|c| c.stdout.as_mut()) {
                Some(stdout) => match stdout.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => { thread::sleep(std::time::Duration::from_millis(10)); continue; }
                },
                None => break,
            }
        };

        let samples: Vec<f32> = (0..n/4).map(|i| {
            f32::from_le_bytes([buf[i*4], buf[i*4+1], buf[i*4+2], buf[i*4+3]])
        }).collect();

        let mut shared = shared.lock().unwrap();
        if shared.len() + samples.len() > 8192 {
            let drain = (shared.len() + samples.len()).saturating_sub(8192);
            if drain >= shared.len() { shared.clear(); } else { shared.drain(0..drain); }
        }
        shared.extend(samples);
    }
}

fn simple_spectrum(samples: &[f32], bands: usize) -> Vec<f32> {
    if samples.is_empty() { return vec![0.0; bands]; }

    let per_band = samples.len() / bands;
    if per_band == 0 { return vec![compute_rms(samples); bands]; }

    (0..bands).map(|i| {
        let start = i * per_band;
        let end = if i == bands - 1 { samples.len() } else { (i + 1) * per_band };
        let rms = compute_rms(&samples[start..end]);
        (rms.sqrt() * 2.0).clamp(0.0, 1.0)
    }).collect()
}

fn compute_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() { return 0.0; }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}
