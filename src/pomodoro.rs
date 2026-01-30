use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PomodoroState {
    Idle,
    Working,
    ShortBreak,
    LongBreak,
}

pub struct PomodoroTimer {
    pub state: PomodoroState,
    pub work_duration: Duration,
    pub short_break: Duration,
    pub long_break: Duration,
    pub sessions_until_long: u8,
    pub current_session: u8,
    pub time_remaining: Duration,
    pub paused: bool,
    last_tick: Instant,
}

impl PomodoroTimer {
    pub fn new() -> Self {
        Self {
            state: PomodoroState::Idle,
            work_duration: Duration::from_secs(25 * 60),
            short_break: Duration::from_secs(5 * 60),
            long_break: Duration::from_secs(15 * 60),
            sessions_until_long: 4,
            current_session: 0,
            time_remaining: Duration::from_secs(25 * 60),
            paused: false,
            last_tick: Instant::now(),
        }
    }

    pub fn start(&mut self) {
        if self.state == PomodoroState::Idle {
            self.state = PomodoroState::Working;
            self.time_remaining = self.work_duration;
            self.current_session = 1;
            self.paused = false;
            self.last_tick = Instant::now();
        }
    }

    pub fn pause(&mut self) {
        if self.state != PomodoroState::Idle {
            self.paused = !self.paused;
            if !self.paused {
                self.last_tick = Instant::now();
            }
        }
    }

    pub fn reset(&mut self) {
        self.state = PomodoroState::Idle;
        self.current_session = 0;
        self.time_remaining = self.work_duration;
        self.paused = false;
        self.last_tick = Instant::now();
    }

    pub fn skip(&mut self) {
        match self.state {
            PomodoroState::Idle => self.start(),
            PomodoroState::Working => self.transition_to_break(),
            PomodoroState::ShortBreak | PomodoroState::LongBreak => self.transition_to_work(),
        }
        self.last_tick = Instant::now();
    }

    pub fn tick(&mut self) -> bool {
        if self.state == PomodoroState::Idle || self.paused {
            self.last_tick = Instant::now();
            return false;
        }

        let now = Instant::now();
        let elapsed = now.duration_since(self.last_tick);
        self.last_tick = now;

        if elapsed >= self.time_remaining {
            self.time_remaining = Duration::ZERO;
            self.auto_transition()
        } else {
            self.time_remaining -= elapsed;
            false
        }
    }

    pub fn format_time(&self) -> String {
        let total_secs = self.time_remaining.as_secs();
        format!("{:02}:{:02}", total_secs / 60, total_secs % 60)
    }

    pub fn progress(&self) -> f32 {
        let total = match self.state {
            PomodoroState::Idle | PomodoroState::Working => self.work_duration,
            PomodoroState::ShortBreak => self.short_break,
            PomodoroState::LongBreak => self.long_break,
        };
        if total.as_secs() == 0 { 0.0 } else {
            self.time_remaining.as_secs_f32() / total.as_secs_f32()
        }
    }

    fn auto_transition(&mut self) -> bool {
        match self.state {
            PomodoroState::Working => self.transition_to_break(),
            PomodoroState::ShortBreak | PomodoroState::LongBreak => self.transition_to_work(),
            PomodoroState::Idle => return false,
        }
        true
    }

    fn transition_to_break(&mut self) {
        if self.current_session >= self.sessions_until_long {
            self.state = PomodoroState::LongBreak;
            self.time_remaining = self.long_break;
            self.current_session = 0;
        } else {
            self.state = PomodoroState::ShortBreak;
            self.time_remaining = self.short_break;
        }
        self.paused = false;
    }

    fn transition_to_work(&mut self) {
        self.state = PomodoroState::Working;
        self.time_remaining = self.work_duration;
        self.current_session += 1;
        self.paused = false;
    }
}

impl Default for PomodoroTimer {
    fn default() -> Self { Self::new() }
}
