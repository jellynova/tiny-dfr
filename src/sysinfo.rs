use std::collections::VecDeque;
use std::fs;
use std::time::Instant;

pub struct SystemStats {
    pub cpu_history: VecDeque<f32>,
    pub ram_history: VecDeque<f32>,
    pub temp_history: VecDeque<f32>,
    history_length: usize,
    pub last_sample: Instant,
    prev_cpu_total: u64,
    prev_cpu_idle: u64,
}

impl SystemStats {
    pub fn new(history_length: usize) -> Self {
        Self {
            cpu_history: VecDeque::with_capacity(history_length),
            ram_history: VecDeque::with_capacity(history_length),
            temp_history: VecDeque::with_capacity(history_length),
            history_length,
            last_sample: Instant::now(),
            prev_cpu_total: 0,
            prev_cpu_idle: 0,
        }
    }

    pub fn sample(&mut self) {
        let cpu = self.read_cpu_usage();
        let ram = read_ram_usage();
        let temp = read_temperature();

        if self.cpu_history.len() >= self.history_length { self.cpu_history.pop_front(); }
        self.cpu_history.push_back(cpu);

        if self.ram_history.len() >= self.history_length { self.ram_history.pop_front(); }
        self.ram_history.push_back(ram);

        if self.temp_history.len() >= self.history_length { self.temp_history.pop_front(); }
        self.temp_history.push_back(temp);

        self.last_sample = Instant::now();
    }

    pub fn get_cpu_percent(&self) -> f32 { self.cpu_history.back().copied().unwrap_or(0.0) }
    pub fn get_ram_percent(&self) -> f32 { self.ram_history.back().copied().unwrap_or(0.0) }
    pub fn get_temp_celsius(&self) -> f32 { self.temp_history.back().copied().unwrap_or(0.0) }

    fn read_cpu_usage(&mut self) -> f32 {
        let content = match fs::read_to_string("/proc/stat") {
            Ok(c) => c,
            Err(_) => return 0.0,
        };

        let cpu_line = match content.lines().find(|l| l.starts_with("cpu ")) {
            Some(line) => line,
            None => return 0.0,
        };

        let values: Vec<u64> = cpu_line.split_whitespace().skip(1)
            .filter_map(|s| s.parse().ok()).collect();

        if values.len() < 4 { return 0.0; }

        let idle = values[3] + values.get(4).unwrap_or(&0);
        let total: u64 = values.iter().sum();

        let total_delta = total.saturating_sub(self.prev_cpu_total);
        let idle_delta = idle.saturating_sub(self.prev_cpu_idle);

        self.prev_cpu_total = total;
        self.prev_cpu_idle = idle;

        if total_delta == 0 { return 0.0; }
        ((total_delta - idle_delta) as f32 / total_delta as f32 * 100.0).clamp(0.0, 100.0)
    }
}

fn read_ram_usage() -> f32 {
    let content = match fs::read_to_string("/proc/meminfo") {
        Ok(c) => c,
        Err(_) => return 0.0,
    };

    let mut mem_total: Option<u64> = None;
    let mut mem_available: Option<u64> = None;

    for line in content.lines() {
        if line.starts_with("MemTotal:") {
            mem_total = line.split_whitespace().nth(1).and_then(|s| s.parse().ok());
        } else if line.starts_with("MemAvailable:") {
            mem_available = line.split_whitespace().nth(1).and_then(|s| s.parse().ok());
        }
        if mem_total.is_some() && mem_available.is_some() { break; }
    }

    match (mem_total, mem_available) {
        (Some(total), Some(available)) if total > 0 => {
            ((total.saturating_sub(available)) as f32 / total as f32 * 100.0).clamp(0.0, 100.0)
        }
        _ => 0.0,
    }
}

fn read_temperature() -> f32 {
    for path in &[
        "/sys/class/thermal/thermal_zone0/temp",
        "/sys/class/thermal/thermal_zone1/temp",
        "/sys/class/hwmon/hwmon0/temp1_input",
    ] {
        if let Ok(content) = fs::read_to_string(path) {
            if let Ok(millidegrees) = content.trim().parse::<i64>() {
                return millidegrees as f32 / 1000.0;
            }
        }
    }
    0.0
}
