use anyhow::{Context, Result, anyhow, bail};
use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, BorderType, Paragraph},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    env, fs,
    io::{self, Read, Stdout, Write},
    net::{TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const APP_NAME: &str = "ComfyTUI";
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    comfy_dir: PathBuf,
    python: PathBuf,
    comfy_args: Vec<String>,
    memory_high: String,
    memory_max: String,
    memory_swap_max: String,
    refresh_ms: u64,
    max_log_lines: usize,
    gpu_index: u32,
    api_host: String,
    api_port: u16,
    auto_stop_on_low_memory: bool,
    emergency_ram_floor_mib: u64,
    emergency_consecutive_samples: u32,
}

impl Default for Config {
    fn default() -> Self {
        let home = home_dir().unwrap_or_else(|| PathBuf::from("/home/daanh"));
        Self {
            comfy_dir: PathBuf::from("/media/daanh/Shared/comfy/ComfyUI"),
            python: home.join(".pyenv/versions/3.13.7/envs/venv13/bin/python"),
            comfy_args: vec![
                "main.py".into(),
                "--lowvram".into(),
                "--preview-method".into(),
                "auto".into(),
            ],
            memory_high: "23G".into(),
            memory_max: "25G".into(),
            memory_swap_max: "3G".into(),
            refresh_ms: 1_000,
            max_log_lines: 20_000,
            gpu_index: 0,
            api_host: "127.0.0.1".to_owned(),
            api_port: 8188,
            auto_stop_on_low_memory: true,
            emergency_ram_floor_mib: 2_048,
            emergency_consecutive_samples: 3,
        }
    }
}

impl Config {
    fn load_or_create() -> Result<(Self, PathBuf)> {
        let path = config_path()?;
        if !path.exists() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            let config = Self::default();
            let serialized = toml::to_string_pretty(&config).context("serializing default config")?;
            fs::write(&path, serialized)
                .with_context(|| format!("writing default config to {}", path.display()))?;
            return Ok((config, path));
        }

        let text = fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let config: Self = toml::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok((config, path))
    }

    fn validate(&self) -> Result<()> {
        if !self.comfy_dir.is_dir() {
            bail!("ComfyUI directory does not exist: {}", self.comfy_dir.display());
        }
        if !self.python.is_file() {
            bail!("Python interpreter does not exist: {}", self.python.display());
        }
        if self.comfy_args.is_empty() {
            bail!("comfy_args cannot be empty");
        }
        if self.refresh_ms < 250 {
            bail!("refresh_ms must be at least 250");
        }
        if self.max_log_lines < 500 {
            bail!("max_log_lines must be at least 500");
        }
        if self.api_host.trim().is_empty() {
            bail!("api_host cannot be empty");
        }
        if self.api_port == 0 {
            bail!("api_port must be greater than 0");
        }
        if self.emergency_consecutive_samples == 0 {
            bail!("emergency_consecutive_samples must be at least 1");
        }
        command_available("systemd-run")?;
        command_available("systemctl")?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
    System,
    Warning,
}

#[derive(Debug, Clone, Default)]
struct ProgressUpdate {
    current: u64,
    total: u64,
}

#[derive(Debug, Clone)]
struct LogEvent {
    kind: StreamKind,
    text: String,
    progress: Option<ProgressUpdate>,
}

#[derive(Debug, Clone, Default)]
struct CpuMetrics {
    usage_pct: f64,
    temp_c: Option<f64>,
    frequency_mhz: Option<f64>,
    load_1: Option<f64>,
    load_5: Option<f64>,
    load_15: Option<f64>,
}

#[derive(Debug, Clone, Default)]
struct MemoryMetrics {
    total: u64,
    available: u64,
    used: u64,
    cached: u64,
    dirty: u64,
    writeback: u64,
    swap_total: u64,
    swap_used: u64,
    swap_in_bytes_per_sec: Option<f64>,
    swap_out_bytes_per_sec: Option<f64>,
    major_faults_per_sec: Option<f64>,
    pressure_some_avg10: Option<f64>,
    pressure_full_avg10: Option<f64>,
    io_pressure_some_avg10: Option<f64>,
    io_pressure_full_avg10: Option<f64>,
}

#[derive(Debug, Clone, Default)]
struct GpuMetrics {
    available: bool,
    name: String,
    utilization_pct: Option<f64>,
    temperature_c: Option<f64>,
    vram_total: Option<u64>,
    vram_used: Option<u64>,
    vram_free: Option<u64>,
    power_draw_w: Option<f64>,
    power_limit_w: Option<f64>,
    fan_pct: Option<f64>,
    graphics_clock_mhz: Option<f64>,
    pstate: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ScopeMetrics {
    active_state: String,
    sub_state: String,
    memory_current: Option<u64>,
    memory_peak: Option<u64>,
    memory_high: Option<u64>,
    memory_max: Option<u64>,
    memory_swap_current: Option<u64>,
    memory_swap_max: Option<u64>,
    cpu_usage_nsec: Option<u64>,
    cpu_usage_pct: Option<f64>,
    tasks_current: Option<u64>,
    io_read_bytes: Option<u64>,
    io_write_bytes: Option<u64>,
    memory_high_events: Option<u64>,
    memory_max_events: Option<u64>,
    oom_events: Option<u64>,
    oom_kill_events: Option<u64>,
}

#[derive(Debug, Clone, Default)]
struct DiskMetrics {
    total: u64,
    used: u64,
    available: u64,
    used_pct: f64,
    mount: String,
}

#[derive(Debug, Clone, Default)]
struct PowerMetrics {
    ac_online: Option<bool>,
    battery_pct: Option<u8>,
    battery_status: Option<String>,
}


#[derive(Debug, Clone, Default)]
struct GenerationMetrics {
    api_connected: bool,
    api_error: Option<String>,
    running: usize,
    pending: usize,
    prompt_number: Option<String>,
    prompt_id: Option<String>,
    node_count: Option<usize>,
    output_node_count: Option<usize>,
    model: Option<String>,
    text_encoder: Option<String>,
    vae: Option<String>,
    loras: Vec<String>,
    steps: Option<u64>,
    current_step: Option<u64>,
    progress_total: Option<u64>,
    cfg: Option<f64>,
    sampler: Option<String>,
    scheduler: Option<String>,
    seed: Option<String>,
    denoise: Option<f64>,
    guidance: Option<f64>,
    shift: Option<f64>,
    width: Option<u64>,
    height: Option<u64>,
    batch_size: Option<u64>,
    started_at: Option<Instant>,
    sampling_started_at: Option<Instant>,
    last_duration: Option<Duration>,
    last_prompt_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct GenerationSnapshot {
    api_connected: bool,
    api_error: Option<String>,
    running: usize,
    pending: usize,
    prompt_number: Option<String>,
    prompt_id: Option<String>,
    node_count: Option<usize>,
    output_node_count: Option<usize>,
    model: Option<String>,
    text_encoder: Option<String>,
    vae: Option<String>,
    loras: Vec<String>,
    steps: Option<u64>,
    cfg: Option<f64>,
    sampler: Option<String>,
    scheduler: Option<String>,
    seed: Option<String>,
    denoise: Option<f64>,
    guidance: Option<f64>,
    shift: Option<f64>,
    width: Option<u64>,
    height: Option<u64>,
    batch_size: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default)]
struct VmCounters {
    swap_in_pages: u64,
    swap_out_pages: u64,
    major_faults: u64,
}

#[derive(Debug, Clone, Default)]
struct Metrics {
    cpu: CpuMetrics,
    memory: MemoryMetrics,
    gpu: GpuMetrics,
    scope: ScopeMetrics,
    disk: DiskMetrics,
    power: PowerMetrics,
}

#[derive(Debug, Clone, Copy, Default)]
struct CpuTimes {
    idle: u64,
    total: u64,
}

struct MetricsCollector {
    previous_cpu: Option<CpuTimes>,
    previous_vm: Option<(VmCounters, Instant)>,
    previous_scope_cpu: Option<(String, u64, Instant)>,
}

impl MetricsCollector {
    fn new() -> Self {
        Self {
            previous_cpu: None,
            previous_vm: None,
            previous_scope_cpu: None,
        }
    }

    fn collect(&mut self, config: &Config, scope_unit: Option<&str>) -> Metrics {
        let mut memory = collect_memory();
        self.apply_vm_rates(&mut memory);

        let mut scope = scope_unit.map(collect_scope).unwrap_or_default();
        self.apply_scope_cpu_rate(scope_unit, &mut scope);

        Metrics {
            cpu: self.collect_cpu(),
            memory,
            gpu: collect_gpu(config.gpu_index),
            scope,
            disk: collect_disk(&config.comfy_dir),
            power: collect_power(),
        }
    }

    fn apply_vm_rates(&mut self, memory: &mut MemoryMetrics) {
        let Some(current) = read_vm_counters() else {
            self.previous_vm = None;
            return;
        };
        let now = Instant::now();
        if let Some((previous, previous_at)) = self.previous_vm.as_ref() {
            let elapsed = previous_at.elapsed().as_secs_f64();
            if elapsed > 0.0 {
                let page_size = 4096.0;
                memory.swap_in_bytes_per_sec = Some(
                    current.swap_in_pages.saturating_sub(previous.swap_in_pages) as f64
                        * page_size
                        / elapsed,
                );
                memory.swap_out_bytes_per_sec = Some(
                    current.swap_out_pages.saturating_sub(previous.swap_out_pages) as f64
                        * page_size
                        / elapsed,
                );
                memory.major_faults_per_sec = Some(
                    current.major_faults.saturating_sub(previous.major_faults) as f64 / elapsed,
                );
            }
        }
        self.previous_vm = Some((current, now));
    }

    fn apply_scope_cpu_rate(&mut self, scope_unit: Option<&str>, scope: &mut ScopeMetrics) {
        let Some(unit) = scope_unit else {
            self.previous_scope_cpu = None;
            return;
        };
        let Some(current_nsec) = scope.cpu_usage_nsec else {
            self.previous_scope_cpu = None;
            return;
        };
        let now = Instant::now();
        if let Some((previous_unit, previous_nsec, previous_at)) = &self.previous_scope_cpu {
            if previous_unit == unit {
                let elapsed_nsec = previous_at.elapsed().as_nanos() as f64;
                if elapsed_nsec > 0.0 {
                    scope.cpu_usage_pct = Some(
                        current_nsec.saturating_sub(*previous_nsec) as f64 / elapsed_nsec * 100.0,
                    );
                }
            }
        }
        self.previous_scope_cpu = Some((unit.to_owned(), current_nsec, now));
    }

    fn collect_cpu(&mut self) -> CpuMetrics {
        let current = read_cpu_times();
        let usage_pct = match (self.previous_cpu, current) {
            (Some(previous), Some(now)) => {
                let total_delta = now.total.saturating_sub(previous.total);
                let idle_delta = now.idle.saturating_sub(previous.idle);
                if total_delta == 0 {
                    0.0
                } else {
                    ((total_delta.saturating_sub(idle_delta)) as f64 / total_delta as f64) * 100.0
                }
            }
            _ => 0.0,
        };
        self.previous_cpu = current;

        let (load_1, load_5, load_15) = read_load_average();
        CpuMetrics {
            usage_pct,
            temp_c: read_cpu_temperature(),
            frequency_mhz: read_average_cpu_frequency_mhz(),
            load_1,
            load_5,
            load_15,
        }
    }
}

struct App {
    config: Config,
    config_path: PathBuf,
    child: Option<Child>,
    scope_unit: Option<String>,
    run_number: u32,
    tx: Sender<LogEvent>,
    rx: Receiver<LogEvent>,
    logs: VecDeque<LogEvent>,
    max_log_lines: usize,
    follow_logs: bool,
    log_offset: usize,
    horizontal_offset: usize,
    status: String,
    metrics: Metrics,
    generation: GenerationMetrics,
    collector: MetricsCollector,
    last_metrics_refresh: Instant,
    launch_error: Option<String>,
    low_memory_samples: u32,
    shutdown_deadline: Option<Instant>,
    shutdown_step: u8,
    quitting: bool,
    restarting: bool,
    command_input: Option<String>,
    show_help: bool,
    insights: Vec<String>,
}

impl App {
    fn new(config: Config, config_path: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        let max_log_lines = config.max_log_lines;
        Self {
            config,
            config_path,
            child: None,
            scope_unit: None,
            run_number: 0,
            tx,
            rx,
            logs: VecDeque::with_capacity(max_log_lines.min(20_000)),
            max_log_lines,
            follow_logs: true,
            log_offset: 0,
            horizontal_offset: 0,
            status: "Initialising".into(),
            metrics: Metrics::default(),
            generation: GenerationMetrics::default(),
            collector: MetricsCollector::new(),
            last_metrics_refresh: Instant::now() - Duration::from_secs(10),
            launch_error: None,
            low_memory_samples: 0,
            shutdown_deadline: None,
            shutdown_step: 0,
            quitting: false,
            restarting: false,
            command_input: None,
            show_help: false,
            insights: Vec::new(),
        }
    }

    fn launch(&mut self) -> Result<()> {
        if self.child.is_some() {
            bail!("ComfyUI is already running under ComfyTUI");
        }

        self.run_number = self.run_number.saturating_add(1);
        let epoch_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let unit_base = format!(
            "comfytui-{}-{}-{epoch_ms}",
            std::process::id(),
            self.run_number
        );
        let scope_unit = format!("{unit_base}.scope");

        let mut command = Command::new("systemd-run");
        command
            .current_dir(&self.config.comfy_dir)
            .env("PYTHONUNBUFFERED", "1")
            .arg("--user")
            .arg("--scope")
            .arg("--collect")
            .arg(format!("--unit={unit_base}"))
            .arg("-p")
            .arg(format!("MemoryHigh={}", self.config.memory_high))
            .arg("-p")
            .arg(format!("MemoryMax={}", self.config.memory_max))
            .arg("-p")
            .arg(format!("MemorySwapMax={}", self.config.memory_swap_max))
            .arg("-p")
            .arg("OOMPolicy=kill")
            .arg(&self.config.python)
            .args(&self.config.comfy_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        self.push_system(format!(
            "Launching {} with MemoryHigh={}, MemoryMax={}, MemorySwapMax={}",
            self.config.python.display(),
            self.config.memory_high,
            self.config.memory_max,
            self.config.memory_swap_max
        ));
        self.push_system(format!("Working directory: {}", self.config.comfy_dir.display()));

        let mut child = command.spawn().context("starting systemd-run")?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("failed to capture stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| anyhow!("failed to capture stderr"))?;
        spawn_log_reader(stdout, StreamKind::Stdout, self.tx.clone());
        spawn_log_reader(stderr, StreamKind::Stderr, self.tx.clone());

        self.status = "Starting".into();
        self.scope_unit = Some(scope_unit);
        self.child = Some(child);
        self.launch_error = None;
        Ok(())
    }

    fn restart(&mut self) {
        self.push_system("Restart requested".into());
        self.restarting = true;
        self.begin_stop(Duration::from_secs(5));
    }

    fn begin_stop(&mut self, timeout: Duration) {
        if self.status == "Stopping" {
            return;
        }

        let Some(unit) = self.scope_unit.clone() else {
            self.status = "Stopped".into();
            return;
        };

        self.status = "Stopping".into();
        self.push_system(format!("Sending SIGINT to {unit}"));
        let _ = systemctl(&[
            "--user",
            "kill",
            "--kill-whom=all",
            "--signal=SIGINT",
            &unit,
        ]);

        self.shutdown_deadline = Some(Instant::now() + timeout);
        self.shutdown_step = 1;
    }

    fn poll_shutdown(&mut self) {
        if self.status != "Stopping" {
            return;
        }

        let Some(unit) = self.scope_unit.clone() else {
            self.finish_stopped();
            return;
        };

        if !scope_is_active(&unit) {
            self.finish_stopped();
            return;
        }

        let deadline = match self.shutdown_deadline {
            Some(d) => d,
            None => return,
        };

        if Instant::now() >= deadline {
            if self.shutdown_step == 1 {
                self.push_warning(format!("{unit} did not stop after SIGINT; asking systemd to stop it"));
                let _ = systemctl(&["--user", "stop", &unit]);
                self.shutdown_step = 2;
                self.shutdown_deadline = Some(Instant::now() + Duration::from_secs(2));
            } else if self.shutdown_step == 2 {
                self.push_warning(format!("{unit} still active; forcing SIGKILL"));
                self.force_stop();
            }
        }
    }

    fn force_stop(&mut self) {
        if let Some(unit) = self.scope_unit.clone() {
            let _ = systemctl(&[
                "--user",
                "kill",
                "--kill-whom=all",
                "--signal=SIGKILL",
                &unit,
            ]);
            let _ = systemctl(&["--user", "stop", &unit]);
            self.push_warning(format!("Force-stopped {unit}"));
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.finish_stopped();
    }

    fn finish_stopped(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.try_wait();
        }
        self.child = None;
        self.scope_unit = None;
        self.status = "Stopped".into();
        self.shutdown_deadline = None;
        self.shutdown_step = 0;
    }

    fn check_child(&mut self) {
        if self.status == "Stopped" {
            return;
        }
        if let Some(mut child) = self.child.take() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.status = "Stopped".into();
                    self.push_system(format!("ComfyUI exited with {status}"));
                    self.scope_unit = None;
                }
                Ok(None) => {
                    self.child = Some(child);
                    if self.status == "Starting" && self.scope_unit.as_deref().is_some_and(scope_is_active) {
                        self.status = "Running".into();
                    }
                }
                Err(e) => {
                    self.push_system(format!("Error waiting on ComfyUI: {e}"));
                    self.status = "Stopped".into();
                    self.scope_unit = None;
                }
            }
        }
    }


    fn drain_logs(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            if let Some(progress) = event.progress.clone() {
                self.apply_generation_progress(progress);
            }
            
            let text_lower = event.text.to_lowercase();
            if text_lower.contains("import failed") 
                || text_lower.contains("exception:") 
                || text_lower.contains("no module named") 
                || text_lower.contains("traceback (most recent call last)")
                || text_lower.contains("error:") {
                let trimmed = event.text.trim().to_owned();
                if trimmed.len() > 10 && !self.insights.contains(&trimmed) {
                    self.insights.push(trimmed);
                    if self.insights.len() > 50 {
                        self.insights.remove(0);
                    }
                }
            }

            self.logs.push_back(event);
            while self.logs.len() > self.max_log_lines {
                self.logs.pop_front();
            }
            if self.follow_logs {
                self.log_offset = 0;
            }
        }
    }

    fn apply_generation_progress(&mut self, progress: ProgressUpdate) {
        if self.generation.running == 0 || progress.total == 0 {
            return;
        }
        if let Some(configured_steps) = self.generation.steps {
            if progress.total != configured_steps {
                return;
            }
        } else if progress.total < 2 {
            return;
        }
        if self.generation.sampling_started_at.is_none() {
            self.generation.sampling_started_at = Some(Instant::now());
        }
        self.generation.current_step = Some(progress.current.min(progress.total));
        self.generation.progress_total = Some(progress.total);
        if self.generation.steps.is_none() {
            self.generation.steps = Some(progress.total);
        }
    }

    fn update_generation_snapshot(&mut self, snapshot: GenerationSnapshot) {
        if !snapshot.api_connected {
            self.generation.api_connected = false;
            self.generation.api_error = snapshot.api_error;
            return;
        }

        let previous_id = self.generation.prompt_id.clone();
        let prompt_changed = previous_id != snapshot.prompt_id;
        if prompt_changed {
            if let (Some(old_id), Some(started_at)) =
                (previous_id, self.generation.started_at.take())
            {
                self.generation.last_duration = Some(started_at.elapsed());
                self.generation.last_prompt_id = Some(old_id);
            }
            self.generation.current_step = None;
            self.generation.progress_total = None;
            self.generation.steps = None;
            self.generation.sampling_started_at = None;
            self.generation.started_at = snapshot.prompt_id.as_ref().map(|_| Instant::now());
        }

        self.generation.api_connected = true;
        self.generation.api_error = None;
        self.generation.running = snapshot.running;
        self.generation.pending = snapshot.pending;
        self.generation.prompt_number = snapshot.prompt_number;
        self.generation.prompt_id = snapshot.prompt_id;
        self.generation.node_count = snapshot.node_count;
        self.generation.output_node_count = snapshot.output_node_count;
        self.generation.model = snapshot.model;
        self.generation.text_encoder = snapshot.text_encoder;
        self.generation.vae = snapshot.vae;
        self.generation.loras = snapshot.loras;
        self.generation.steps = snapshot.steps.or(self.generation.steps);
        self.generation.cfg = snapshot.cfg;
        self.generation.sampler = snapshot.sampler;
        self.generation.scheduler = snapshot.scheduler;
        self.generation.seed = snapshot.seed;
        self.generation.denoise = snapshot.denoise;
        self.generation.guidance = snapshot.guidance;
        self.generation.shift = snapshot.shift;
        self.generation.width = snapshot.width;
        self.generation.height = snapshot.height;
        self.generation.batch_size = snapshot.batch_size;
    }

    fn refresh_metrics_if_due(&mut self) {
        if self.last_metrics_refresh.elapsed() < Duration::from_millis(self.config.refresh_ms) {
            return;
        }
        self.metrics = self
            .collector
            .collect(&self.config, self.scope_unit.as_deref());
        let generation = collect_generation_snapshot(&self.config);
        self.update_generation_snapshot(generation);
        self.last_metrics_refresh = Instant::now();
        self.apply_low_memory_watchdog();
    }

    fn apply_low_memory_watchdog(&mut self) {
        if !self.config.auto_stop_on_low_memory || self.child.is_none() {
            self.low_memory_samples = 0;
            return;
        }

        let floor = self
            .config
            .emergency_ram_floor_mib
            .saturating_mul(1024 * 1024);
        let available = self.metrics.memory.available;
        if available > 0 && available < floor {
            self.low_memory_samples = self.low_memory_samples.saturating_add(1);
        } else {
            self.low_memory_samples = 0;
        }

        if self.low_memory_samples >= self.config.emergency_consecutive_samples {
            self.push_warning(format!(
                "Emergency watchdog stopping ComfyUI: only {} RAM available (floor {})",
                human_bytes(available),
                human_bytes(floor)
            ));
            self.low_memory_samples = 0;
            self.begin_stop(Duration::from_secs(3));
        }
    }

    fn push_system(&mut self, text: String) {
        self.logs.push_back(LogEvent { kind: StreamKind::System, text, progress: None });
        self.trim_logs();
    }

    fn push_warning(&mut self, text: String) {
        self.logs.push_back(LogEvent { kind: StreamKind::Warning, text, progress: None });
        self.trim_logs();
    }

    fn trim_logs(&mut self) {
        while self.logs.len() > self.max_log_lines {
            self.logs.pop_front();
        }
    }

    fn scroll_up(&mut self, amount: usize) {
        self.follow_logs = false;
        self.log_offset = self
            .log_offset
            .saturating_add(amount)
            .min(self.logs.len().saturating_sub(1));
    }

    fn scroll_down(&mut self, amount: usize) {
        self.log_offset = self.log_offset.saturating_sub(amount);
        if self.log_offset == 0 {
            self.follow_logs = true;
        }
    }

    fn clear_logs(&mut self) {
        self.logs.clear();
        self.log_offset = 0;
        self.horizontal_offset = 0;
        self.follow_logs = true;
        self.push_system("Log buffer cleared".into());
    }

    fn warning_summary(&self) -> Option<String> {
        let mut warnings = Vec::new();
        if self.metrics.memory.available > 0 && self.metrics.memory.available < gib(2) {
            warnings.push(format!(
                "LOW RAM: {} available",
                human_bytes(self.metrics.memory.available)
            ));
        }
        if self.metrics.memory.pressure_full_avg10.is_some_and(|value| value >= 5.0) {
            warnings.push(format!(
                "MEMORY STALLS: full PSI {:.1}%",
                self.metrics.memory.pressure_full_avg10.unwrap_or_default()
            ));
        }
        let swap_io = self
            .metrics
            .memory
            .swap_in_bytes_per_sec
            .unwrap_or(0.0)
            .max(self.metrics.memory.swap_out_bytes_per_sec.unwrap_or(0.0));
        if swap_io >= 64.0 * 1024.0 * 1024.0 {
            warnings.push(format!("SWAP THRASH: {}", format_rate(Some(swap_io))));
        }
        if self
            .metrics
            .memory
            .io_pressure_full_avg10
            .is_some_and(|value| value >= 10.0)
        {
            warnings.push(format!(
                "I/O STALLS: full PSI {:.1}%",
                self.metrics.memory.io_pressure_full_avg10.unwrap_or_default()
            ));
        }
        if let Some(temp) = self.metrics.gpu.temperature_c {
            if temp >= 87.0 {
                warnings.push(format!("GPU HOT: {temp:.0}°C"));
            }
        }
        if let Some(temp) = self.metrics.cpu.temp_c {
            if temp >= 95.0 {
                warnings.push(format!("CPU HOT: {temp:.0}°C"));
            }
        }
        if let (Some(current), Some(max)) = (
            self.metrics.scope.memory_current,
            self.metrics.scope.memory_max,
        ) {
            if max > 0 && current as f64 / max as f64 >= 0.90 {
                warnings.push(format!(
                    "CGROUP {:.0}% OF MAX",
                    current as f64 / max as f64 * 100.0
                ));
            }
        }
        if self.metrics.scope.oom_kill_events.unwrap_or(0) > 0 {
            warnings.push(format!(
                "CGROUP OOM KILLS: {}",
                self.metrics.scope.oom_kill_events.unwrap_or(0)
            ));
        }
        if let Some(error) = &self.launch_error {
            warnings.push(error.clone());
        }
        (!warnings.is_empty()).then(|| warnings.join(" | "))
    }

    fn handle_command(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        if cmd.is_empty() {
            return;
        }
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        match parts[0] {
            "q" | "quit" => {
                self.quitting = true;
                self.begin_stop(Duration::from_secs(5));
            }
            "help" | "h" => {
                self.show_help = true;
            }
            "w" | "write" => {
                self.save_config();
            }
            "wq" => {
                self.save_config();
                self.quitting = true;
                self.begin_stop(Duration::from_secs(5));
            }
            "set" => {
                if parts.len() < 2 {
                    self.push_warning("Usage: :set key=value".into());
                    return;
                }
                let assignment = parts[1..].join(" ");
                if let Some((key, value)) = assignment.split_once('=') {
                    self.apply_setting(key.trim(), value.trim());
                } else {
                    self.push_warning(format!("Invalid setting format: {}", assignment));
                }
            }
            _ => {
                self.push_warning(format!("Unknown command: {}", parts[0]));
            }
        }
    }

    fn apply_setting(&mut self, key: &str, value: &str) {
        let success = match key {
            "memory_high" => {
                self.config.memory_high = value.to_owned();
                true
            }
            "memory_max" => {
                self.config.memory_max = value.to_owned();
                true
            }
            "memory_swap_max" => {
                self.config.memory_swap_max = value.to_owned();
                true
            }
            "auto_stop_on_low_memory" => {
                if let Ok(b) = value.parse::<bool>() {
                    self.config.auto_stop_on_low_memory = b;
                    true
                } else {
                    self.push_warning("Invalid boolean for auto_stop_on_low_memory".into());
                    false
                }
            }
            _ => {
                self.push_warning(format!("Unknown setting: {}", key));
                false
            }
        };

        if success {
            self.push_system(format!("Setting updated: {} = {}", key, value));
        }
    }

    fn save_config(&mut self) {
        match toml::to_string_pretty(&self.config) {
            Ok(serialized) => {
                if let Err(e) = fs::write(&self.config_path, serialized) {
                    self.push_warning(format!("Failed to save config: {}", e));
                } else {
                    self.push_system(format!("Config saved to {}", self.config_path.display()));
                }
            }
            Err(e) => {
                self.push_warning(format!("Failed to serialize config: {}", e));
            }
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(unit) = self.scope_unit.as_deref() {
            let _ = Command::new("systemctl")
                .args([
                    "--user",
                    "kill",
                    "--kill-whom=all",
                    "--signal=SIGINT",
                    unit,
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = Command::new("systemctl")
                .args(["--user", "stop", unit])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("enabling terminal raw mode")?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, Hide) {
            let _ = disable_raw_mode();
            return Err(error).context("entering alternate screen");
        }

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = disable_raw_mode();
                let mut stdout = io::stdout();
                let _ = execute!(stdout, Show, LeaveAlternateScreen);
                return Err(error).context("creating terminal");
            }
        };

        if let Err(error) = terminal.clear() {
            let _ = disable_raw_mode();
            let _ = execute!(terminal.backend_mut(), Show, LeaveAlternateScreen);
            return Err(error).context("clearing terminal");
        }
        Ok(Self { terminal })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), Show, LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

fn main() -> Result<()> {
    if let Some(argument) = env::args().nth(1) {
        match argument.as_str() {
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            "-V" | "--version" => {
                println!("{APP_NAME} {VERSION}");
                return Ok(());
            }
            "--check" => {
                return run_preflight_check();
            }
            "--print-config" => {
                println!("{}", config_path()?.display());
                return Ok(());
            }
            unknown => bail!("unknown argument: {unknown}. Use --help."),
        }
    }

    let (config, config_path) = Config::load_or_create()?;
    config.validate().with_context(|| {
        format!(
            "invalid configuration; edit {}",
            config_path.display()
        )
    })?;

    let mut app = App::new(config, config_path);
    let mut session = TerminalSession::enter()?;

    if let Err(error) = app.launch() {
        let message = format!("Initial launch failed: {error:#}");
        app.launch_error = Some(message.clone());
        app.push_warning(message);
        app.status = "Launch failed".into();
    }

    let result = run_ui(&mut session.terminal, &mut app);

    if app.child.is_some() || app.scope_unit.as_deref().is_some_and(scope_is_active) {
        app.begin_stop(Duration::from_secs(5));
        while app.status != "Stopped" {
            app.drain_logs();
            app.check_child();
            app.poll_shutdown();
            let _ = session.terminal.draw(|frame| render(frame, &app));
            thread::sleep(Duration::from_millis(100));
        }
    }

    result
}


fn print_help() {
    println!(
        "{APP_NAME} {VERSION}\n\n\
Usage:\n  comfytui                 Launch ComfyUI and open the dashboard\n  comfytui --check         Validate paths, NVIDIA access, and cgroup support\n  comfytui --print-config  Print the active config path\n  comfytui --version       Print the version\n  comfytui --help          Show this help"
    );
}

fn run_preflight_check() -> Result<()> {
    let (config, path) = Config::load_or_create()?;
    println!("Config: {}", path.display());
    config.validate()?;
    println!("[ok] ComfyUI directory: {}", config.comfy_dir.display());
    println!("[ok] Python interpreter: {}", config.python.display());
    println!("[ok] systemd-run and systemctl are available");
    println!("[ok] ComfyUI API target: http://{}:{}", config.api_host, config.api_port);

    match Command::new("nvidia-smi")
        .args(["--query-gpu=name,memory.total", "--format=csv,noheader"])
        .output()
    {
        Ok(output) if output.status.success() => {
            println!("[ok] NVIDIA: {}", String::from_utf8_lossy(&output.stdout).trim());
        }
        Ok(output) => {
            println!("[warn] nvidia-smi failed: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Err(error) => println!("[warn] nvidia-smi unavailable: {error}"),
    }

    let unit = format!("comfytui-preflight-{}", std::process::id());
    let status = Command::new("systemd-run")
        .arg("--user")
        .arg("--scope")
        .arg("--quiet")
        .arg("--collect")
        .arg(format!("--unit={unit}"))
        .arg("-p")
        .arg("MemoryHigh=32M")
        .arg("-p")
        .arg("MemoryMax=64M")
        .arg("-p")
        .arg("MemorySwapMax=16M")
        .arg("-p")
        .arg("OOMPolicy=kill")
        .arg("true")
        .status()
        .context("testing a transient user scope")?;
    if !status.success() {
        bail!("systemd transient-scope/cgroup test failed with {status}");
    }
    println!("[ok] systemd user scope and memory-controller properties work");
    println!("Preflight complete.");
    Ok(())
}

fn run_ui(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    loop {
        app.drain_logs();
        app.check_child();
        app.refresh_metrics_if_due();
        app.poll_shutdown();

        if app.quitting && app.status == "Stopped" {
            return Ok(());
        }
        
        if app.restarting && app.status == "Stopped" {
            app.restarting = false;
            if let Err(error) = app.launch() {
                let message = format!("Restart failed: {error:#}");
                app.launch_error = Some(message.clone());
                app.push_warning(message);
                app.status = "Launch failed".into();
            }
        }

        terminal.draw(|frame| render(frame, app))?;

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        if app.show_help {
            app.show_help = false;
            continue;
        }

        if let Some(mut input) = app.command_input.take() {
            match key.code {
                KeyCode::Esc => {
                    // Canceled
                }
                KeyCode::Enter => {
                    app.handle_command(&input);
                }
                KeyCode::Backspace => {
                    input.pop();
                    app.command_input = Some(input);
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    app.command_input = Some(input);
                }
                _ => {
                    app.command_input = Some(input);
                }
            }
            continue;
        }

        match key {
            KeyEvent {
                code: KeyCode::Char('q'),
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                app.quitting = true;
                app.begin_stop(Duration::from_secs(5));
            }
            KeyEvent {
                code: KeyCode::Char('s'),
                ..
            } => app.begin_stop(Duration::from_secs(5)),
            KeyEvent {
                code: KeyCode::Char('k'),
                ..
            } => app.force_stop(),
            KeyEvent {
                code: KeyCode::Char('r'),
                ..
            } => app.restart(),
            KeyEvent {
                code: KeyCode::Char('c'),
                ..
            } => app.clear_logs(),
            KeyEvent {
                code: KeyCode::Char(':'),
                ..
            } => app.command_input = Some(String::new()),
            KeyEvent {
                code: KeyCode::Char(' '),
                ..
            } => {
                app.follow_logs = !app.follow_logs;
                if app.follow_logs {
                    app.log_offset = 0;
                }
            }
            KeyEvent {
                code: KeyCode::Up, ..
            } => app.scroll_up(1),
            KeyEvent {
                code: KeyCode::Down,
                ..
            } => app.scroll_down(1),
            KeyEvent {
                code: KeyCode::PageUp,
                ..
            } => app.scroll_up(20),
            KeyEvent {
                code: KeyCode::PageDown,
                ..
            } => app.scroll_down(20),
            KeyEvent {
                code: KeyCode::Home,
                ..
            } => {
                app.follow_logs = false;
                app.log_offset = app.logs.len().saturating_sub(1);
            }
            KeyEvent {
                code: KeyCode::End, ..
            } => {
                app.follow_logs = true;
                app.log_offset = 0;
            }
            KeyEvent {
                code: KeyCode::Left,
                ..
            } => app.horizontal_offset = app.horizontal_offset.saturating_sub(4),
            KeyEvent {
                code: KeyCode::Right,
                ..
            } => app.horizontal_offset = app.horizontal_offset.saturating_add(4),
            _ => {}
        }
    }
}

fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(frame, rows[0], app);

    if area.width < 120 {
        let columns = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[1]);
        render_diagnostics(frame, columns[0], app);
        if app.insights.is_empty() {
            render_logs(frame, columns[1], app);
        } else {
            let log_split = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(25), Constraint::Percentage(75)])
                .split(columns[1]);
            render_insights(frame, log_split[0], app);
            render_logs(frame, log_split[1], app);
        }
    } else {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
            .split(rows[1]);
        render_diagnostics(frame, columns[0], app);
        if app.insights.is_empty() {
            render_logs(frame, columns[1], app);
        } else {
            let log_split = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(25), Constraint::Percentage(75)])
                .split(columns[1]);
            render_insights(frame, log_split[0], app);
            render_logs(frame, log_split[1], app);
        }
    }

    render_footer(frame, rows[2], app);

    if app.show_help {
        render_help_modal(frame, area);
    }
}

fn render_help_modal(frame: &mut Frame, area: Rect) {
    let help_text = vec![
        Line::from(Span::styled("ComfyTUI Help", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from(Span::styled("Commands (press ':' to enter):", Style::default().add_modifier(Modifier::BOLD))),
        Line::from("  :q, :quit                 Stop ComfyUI and exit ComfyTUI"),
        Line::from("  :w, :write                Save the current config"),
        Line::from("  :wq                       Save config and exit"),
        Line::from("  :set key=value            Update a setting (e.g., :set memory_high=16G)"),
        Line::from("  :help, :h                 Show this help menu"),
        Line::from(""),
        Line::from(Span::styled("Keybindings:", Style::default().add_modifier(Modifier::BOLD))),
        Line::from("  q, Ctrl-C                 Quit"),
        Line::from("  s                         Stop ComfyUI gracefully"),
        Line::from("  k                         Force kill ComfyUI"),
        Line::from("  r                         Restart ComfyUI"),
        Line::from("  space                     Toggle log following"),
        Line::from("  c                         Clear logs"),
        Line::from("  Up/Down/PgUp/PgDn/Home    Scroll logs vertically"),
        Line::from("  Left/Right                Scroll logs horizontally"),
        Line::from(""),
        Line::from(Span::styled("Press any key to close this help modal.", Style::default().fg(Color::DarkGray))),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Help ")
        .style(Style::default().bg(Color::Black)); // Set background to black to overlay correctly

    let paragraph = Paragraph::new(help_text).block(block);

    // Calculate a centered rectangle
    let vertical_center = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(20),
            Constraint::Length(22),
            Constraint::Percentage(20),
        ])
        .split(area);

    let horizontal_center = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(20),
            Constraint::Min(70),
            Constraint::Percentage(20),
        ])
        .split(vertical_center[1]);

    use ratatui::widgets::Clear;
    frame.render_widget(Clear, horizontal_center[1]);
    frame.render_widget(paragraph, horizontal_center[1]);
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let status_style = match app.status.as_str() {
        "Running" => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        "Starting" | "Stopping" => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        "Failed" | "Launch failed" => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        _ => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    };

    let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let spinner_frame = spinner[(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() / 100) as usize % spinner.len()];
    
    let status_text = match app.status.as_str() {
        "Starting" => format!("{} Starting", spinner_frame),
        "Stopping" => format!("{} Stopping", spinner_frame),
        other => other.to_owned(),
    };

    let unit = app.scope_unit.as_deref().unwrap_or("no active scope");
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!(" {APP_NAME} v{VERSION} "),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw("│ "),
        Span::styled(status_text, status_style),
        Span::raw(format!(" │ {unit}")),
    ])];

    if let Some(warning) = app.warning_summary() {
        lines.push(Line::from(Span::styled(
            format!(" ⚠ {warning}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!(" Config: {}", app.config_path.display()),
            Style::default().fg(Color::DarkGray),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(" Session ")
        ),
        area,
    );
}

fn render_diagnostics(frame: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(9),
            Constraint::Length(7),
            Constraint::Min(8),
        ])
        .split(area);

    render_generation(frame, rows[0], app);
    render_gpu(frame, rows[1], &app.metrics);
    render_memory(frame, rows[2], &app.metrics);
    render_cpu(frame, rows[3], &app.metrics);
    render_guard(frame, rows[4], app);
}

fn render_generation(frame: &mut Frame, area: Rect, app: &App) {
    let generation = &app.generation;
    let available_width = area.width.saturating_sub(4) as usize;
    let state = if !generation.api_connected {
        if app.status == "Starting" {
            "Waiting for API"
        } else {
            "API unavailable"
        }
    } else if generation.running > 0 {
        if generation.current_step.unwrap_or(0) > 0 {
            "Sampling"
        } else {
            "Preparing / loading"
        }
    } else if generation.pending > 0 {
        "Queued"
    } else {
        "Idle"
    };

    let current = generation.current_step.unwrap_or(0);
    let total = generation
        .progress_total
        .or(generation.steps)
        .unwrap_or(0);
    let progress_pct = if total == 0 {
        0.0
    } else {
        current as f64 / total as f64 * 100.0
    };
    let step_text = if total > 0 {
        format!("{current}/{total}")
    } else {
        "-/-".to_owned()
    };
    let elapsed = generation
        .started_at
        .as_ref()
        .map(|started| format_duration(started.elapsed()))
        .unwrap_or_else(|| "--:--".to_owned());
    let eta = generation_eta(generation)
        .map(format_duration)
        .unwrap_or_else(|| "--:--".to_owned());
    let sampler = generation.sampler.as_deref().unwrap_or("?");
    let scheduler = generation.scheduler.as_deref().unwrap_or("?");
    let cfg = generation
        .cfg
        .map_or_else(|| "?".to_owned(), |value| format!("{value:.2}"));
    let denoise = generation
        .denoise
        .map_or_else(|| "?".to_owned(), |value| format!("{value:.2}"));
    let guidance = generation
        .guidance
        .map_or_else(|| "?".to_owned(), |value| format!("{value:.2}"));
    let shift = generation
        .shift
        .map_or_else(|| "?".to_owned(), |value| format!("{value:.2}"));
    let seed = generation.seed.as_deref().unwrap_or("?");
    let resolution = match (generation.width, generation.height) {
        (Some(width), Some(height)) => format!("{width}×{height}"),
        _ => "?×?".to_owned(),
    };
    let batch = generation.batch_size.unwrap_or(1);
    let nodes = generation
        .node_count
        .map_or_else(|| "?".to_owned(), |value| value.to_string());
    let output_nodes = generation
        .output_node_count
        .map_or_else(|| "?".to_owned(), |value| value.to_string());
    let prompt_label = generation
        .prompt_number
        .as_deref()
        .map_or_else(|| "#?".to_owned(), |value| format!("#{value}"));
    let prompt_id = generation
        .prompt_id
        .as_deref()
        .map_or_else(|| "--------".to_owned(), short_prompt_id);
    let model = generation.model.as_deref().unwrap_or("not detected");
    let encoder = generation.text_encoder.as_deref().unwrap_or("not detected");
    let vae = generation.vae.as_deref().unwrap_or("not detected");
    let loras = if generation.loras.is_empty() {
        "none".to_owned()
    } else {
        generation.loras.join(", ")
    };

    let mut lines = vec![
        Line::from(vec![
            Span::raw(" "),
            Span::styled(state, generation_state_style(state)),
            Span::raw(format!(
                " │ {prompt_label} {prompt_id} │ q {}+{}",
                generation.running, generation.pending
            )),
        ]),
        Line::from(vec![
            Span::raw(" "),
            Span::styled(mini_bar(progress_pct, 18), Style::default().fg(Color::Cyan)),
            Span::raw(format!(" {progress_pct:5.1}% │ step {step_text}")),
        ]),
        Line::from(format!(" Time {elapsed} │ ETA {eta}")),
        Line::from(format!(" Sampler {sampler} │ {scheduler}")),
        Line::from(format!(" CFG {cfg} │ den {denoise} │ g {guidance} │ shift {shift}")),
        Line::from(format!(" Seed {}", truncate_middle(seed, available_width.saturating_sub(7)))),
        Line::from(format!(" Image {resolution} │ batch {batch} │ nodes {nodes}/{output_nodes}")),
        Line::from(format!(" Model {}", truncate_middle(model, available_width.saturating_sub(8)))),
        Line::from(format!(" Encoder {}", truncate_middle(encoder, available_width.saturating_sub(10)))),
        Line::from(format!(
            " VAE {} │ LoRA {}",
            truncate_middle(vae, (available_width / 3).saturating_sub(5)),
            truncate_middle(&loras, (available_width - available_width / 3).saturating_sub(10))
        )),
    ];

    if !generation.api_connected {
        if let Some(error) = &generation.api_error {
            lines[9] = Line::from(Span::styled(
                format!(" API {}", truncate_middle(error, available_width.saturating_sub(6))),
                Style::default().fg(Color::DarkGray),
            ));
        }
    } else if generation.running == 0 {
        if let Some(duration) = generation.last_duration {
            lines[2] = Line::from(format!(" Last generation: {}", format_duration(duration)));
        }
    }

    frame.render_widget(panel(" GENERATION ", lines), area);
}

fn render_cpu(frame: &mut Frame, area: Rect, metrics: &Metrics) {
    let cpu = &metrics.cpu;
    let temp = cpu
        .temp_c
        .map_or_else(|| "N/A".to_owned(), |v| format!("{v:.0}°C"));
    let frequency = cpu
        .frequency_mhz
        .map_or_else(|| "N/A".to_owned(), |v| format!("{v:.0} MHz"));
    let load = match (cpu.load_1, cpu.load_5, cpu.load_15) {
        (Some(a), Some(b), Some(c)) => format!("{a:.2} / {b:.2} / {c:.2}"),
        _ => "N/A".to_owned(),
    };
    let ac: String = metrics.power.ac_online.map_or_else(
        || "AC ?".to_owned(),
        |online| {
            if online {
                "AC online".to_owned()
            } else {
                "Battery".to_owned()
            }
        },
    );
    let battery = metrics.power.battery_pct.map_or_else(String::new, |pct| {
        format!(" │ {pct}% {}", metrics.power.battery_status.as_deref().unwrap_or(""))
    });

    let lines = vec![
        usage_line("CPU", cpu.usage_pct),
        Line::from(format!(" Temp {temp:<8} │ Avg clock {frequency}")),
        Line::from(format!(" Load 1/5/15: {load}")),
        Line::from(format!(" Power: {ac}{battery}")),
    ];
    frame.render_widget(panel(" CPU / SYSTEM ", lines), area);
}

fn render_gpu(frame: &mut Frame, area: Rect, metrics: &Metrics) {
    let gpu = &metrics.gpu;
    if !gpu.available {
        let message = gpu.error.as_deref().unwrap_or("nvidia-smi unavailable");
        frame.render_widget(
            panel(
                " NVIDIA GPU ",
                vec![
                    Line::from(Span::styled(" GPU metrics unavailable", Style::default().fg(Color::Red))),
                    Line::from(format!(" {message}")),
                ],
            ),
            area,
        );
        return;
    }

    let util = gpu.utilization_pct.unwrap_or(0.0);
    let vram_pct = percent(gpu.vram_used.unwrap_or(0), gpu.vram_total.unwrap_or(0));
    let temp = gpu
        .temperature_c
        .map_or_else(|| "N/A".to_owned(), |v| format!("{v:.0}°C"));
    let pstate = gpu.pstate.as_deref().unwrap_or("N/A");
    let used = gpu.vram_used.map_or_else(|| "N/A".to_owned(), human_bytes);
    let free = gpu.vram_free.map_or_else(|| "N/A".to_owned(), human_bytes);
    let power = gpu
        .power_draw_w
        .map_or_else(|| "N/A".to_owned(), |v| format!("{v:.1} W"));
    let limit = gpu
        .power_limit_w
        .map_or_else(|| "N/A".to_owned(), |v| format!("{v:.0} W"));
    let clock = gpu
        .graphics_clock_mhz
        .map_or_else(|| "N/A".to_owned(), |v| format!("{v:.0} MHz"));
    let fan = gpu
        .fan_pct
        .map_or_else(|| "N/A".to_owned(), |v| format!("{v:.0}%"));

    let lines = vec![
        Line::from(Span::styled(
            format!(" {}", gpu.name),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
        usage_line("GPU", util),
        usage_line("VRAM", vram_pct),
        Line::from(format!(" VRAM {used} used │ {free} free")),
        Line::from(format!(" Temp {temp} │ {pstate} │ {clock}")),
        Line::from(format!(" Power {power}/{limit} │ Fan {fan}")),
    ];
    frame.render_widget(panel(" NVIDIA GPU ", lines), area);
}

fn render_memory(frame: &mut Frame, area: Rect, metrics: &Metrics) {
    let memory = &metrics.memory;
    let ram_pct = percent(memory.used, memory.total);
    let swap_pct = percent(memory.swap_used, memory.swap_total);
    let disk = &metrics.disk;

    let lines = vec![
        usage_line("RAM", ram_pct),
        Line::from(Span::styled(
            format!(
                " Available {} │ cache {}",
                human_bytes(memory.available),
                human_bytes(memory.cached)
            ),
            availability_style(percent(memory.available, memory.total)),
        )),
        usage_line("SWAP", swap_pct),
        Line::from(format!(
            " Swap I/O ↓{} ↑{}",
            format_rate(memory.swap_in_bytes_per_sec),
            format_rate(memory.swap_out_bytes_per_sec)
        )),
        Line::from(format!(
            " Major faults {}/s │ dirty {}",
            format_optional_rate(memory.major_faults_per_sec),
            human_bytes(memory.dirty.saturating_add(memory.writeback))
        )),
        Line::from(format!(
            " PSI mem {}/{}% │ I/O {}/{}%",
            format_optional_decimal(memory.pressure_some_avg10),
            format_optional_decimal(memory.pressure_full_avg10),
            format_optional_decimal(memory.io_pressure_some_avg10),
            format_optional_decimal(memory.io_pressure_full_avg10)
        )),
        Line::from(format!(
            " Disk {} free │ {:.0}% used",
            human_bytes(disk.available),
            disk.used_pct
        )),
    ];
    frame.render_widget(panel(" MEMORY / STORAGE ", lines), area);
}

fn render_guard(frame: &mut Frame, area: Rect, app: &App) {
    let scope = &app.metrics.scope;
    let current = scope.memory_current.map_or_else(|| "N/A".to_owned(), human_bytes);
    let peak = scope.memory_peak.map_or_else(|| "N/A".to_owned(), human_bytes);
    let high = scope
        .memory_high
        .map_or_else(|| app.config.memory_high.clone(), human_bytes);
    let max = scope
        .memory_max
        .map_or_else(|| app.config.memory_max.clone(), human_bytes);
    let swap = scope
        .memory_swap_current
        .map_or_else(|| "N/A".to_owned(), human_bytes);
    let headroom = match (scope.memory_current, scope.memory_max) {
        (Some(current), Some(max)) => human_bytes(max.saturating_sub(current)),
        _ => "N/A".to_owned(),
    };
    let state = if scope.active_state.is_empty() {
        app.status.clone()
    } else {
        format!("{} / {}", scope.active_state, scope.sub_state)
    };
    let scope_cpu = scope
        .cpu_usage_pct
        .map_or_else(|| "N/A".to_owned(), |value| format!("{value:.0}%"));
    let tasks = scope
        .tasks_current
        .map_or_else(|| "N/A".to_owned(), |value| value.to_string());

    let lines = vec![
        Line::from(format!(" {state} │ RAM {current} │ peak {peak}")),
        Line::from(format!(" High {high} │ max {max} │ free {headroom}")),
        Line::from(format!(" Scope CPU {scope_cpu} │ tasks {tasks} │ swap {swap}")),
        Line::from(format!(
            " Scope I/O ↓{} ↑{}",
            scope.io_read_bytes.map_or_else(|| "N/A".to_owned(), human_bytes),
            scope.io_write_bytes.map_or_else(|| "N/A".to_owned(), human_bytes)
        )),
        Line::from(format!(
            " Events high {} │ max {} │ OOM kills {}",
            format_optional_u64(scope.memory_high_events),
            format_optional_u64(scope.memory_max_events),
            format_optional_u64(scope.oom_kill_events)
        )),
        Line::from(format!(
            " Watchdog {} │ floor {} │ {}/{}",
            if app.config.auto_stop_on_low_memory { "ON" } else { "OFF" },
            human_bytes(app.config.emergency_ram_floor_mib.saturating_mul(1024 * 1024)),
            app.low_memory_samples,
            app.config.emergency_consecutive_samples
        )),
    ];
    frame.render_widget(panel(" COMFYUI RESOURCE GUARD ", lines), area);
}

fn render_insights(frame: &mut Frame, area: Rect, app: &App) {
    let lines = app.insights.iter().map(|msg| {
        Line::from(Span::styled(format!("• {}", msg), Style::default().fg(Color::Yellow)))
    }).collect::<Vec<_>>();
    frame.render_widget(panel(" INSIGHTS / ISSUES ", lines), area);
}

fn render_logs(frame: &mut Frame, area: Rect, app: &App) {
    let inner_height = area.height.saturating_sub(2) as usize;
    let end = app.logs.len().saturating_sub(app.log_offset);
    let start = end.saturating_sub(inner_height);

    let lines = app
        .logs
        .iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(|event| {
            let prefix = match event.kind {
                StreamKind::Stdout => "",
                StreamKind::Stderr => "[ERR] ",
                StreamKind::System => "[CT]  ",
                StreamKind::Warning => "[WARN] ",
            };
            let complete = format!("{prefix}{}", event.text);
            let base_style = match event.kind {
                StreamKind::Stdout => Style::default(),
                StreamKind::Stderr => Style::default().fg(Color::LightYellow),
                StreamKind::System => Style::default().fg(Color::Cyan),
                StreamKind::Warning => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            };
            
            let mut spans = Vec::new();
            let mut current = String::new();
            for c in complete.chars() {
                if c.is_whitespace() || c == '[' || c == ']' || c == '(' || c == ')' || c == ':' || c == ',' {
                    if !current.is_empty() {
                        let style = match current.as_str() {
                            "Error" | "Failed" | "FAILED" | "ERROR" | "Exception" => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                            "Warning" | "WARNING" | "WARN" => Style::default().fg(Color::LightYellow),
                            "Info" | "INFO" | "ok" | "OK" => Style::default().fg(Color::Green),
                            s if s.parse::<f64>().is_ok() => Style::default().fg(Color::LightCyan),
                            s if s.starts_with('/') || s.starts_with("C:\\") || s.starts_with("http://") || s.starts_with("https://") => Style::default().fg(Color::LightBlue),
                            _ => base_style,
                        };
                        spans.push(Span::styled(current.clone(), style));
                        current.clear();
                    }
                    spans.push(Span::styled(c.to_string(), base_style));
                } else {
                    current.push(c);
                }
            }
            if !current.is_empty() {
                let style = match current.as_str() {
                    "Error" | "Failed" | "FAILED" | "ERROR" | "Exception" => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    "Warning" | "WARNING" | "WARN" => Style::default().fg(Color::LightYellow),
                    "Info" | "INFO" | "ok" | "OK" => Style::default().fg(Color::Green),
                    s if s.parse::<f64>().is_ok() => Style::default().fg(Color::LightCyan),
                    s if s.starts_with('/') || s.starts_with("C:\\") || s.starts_with("http://") || s.starts_with("https://") => Style::default().fg(Color::LightBlue),
                    _ => base_style,
                };
                spans.push(Span::styled(current, style));
            }
            Line::from(spans)
        })
        .collect::<Vec<_>>();

    let mode = if app.follow_logs { "FOLLOW" } else { "PAUSED" };
    let title = format!(
        " COMFYUI LOGS │ {mode} │ vertical -{} │ horizontal +{} ",
        app.log_offset, app.horizontal_offset
    );
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(Span::styled(title, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)))
            )
            .scroll((0, app.horizontal_offset as u16)),
        area,
    );
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let line = if let Some(input) = &app.command_input {
        Line::from(vec![
            Span::styled(":", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw(input.clone()),
            Span::styled("█", Style::default().fg(Color::Gray)),
        ])
    } else {
        Line::from(vec![
            Span::styled(" q", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(" quit  "),
            Span::styled("s", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(" stop  "),
            Span::styled("r", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(" restart  "),
            Span::styled("k", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::raw(" force-kill  "),
            Span::styled("space", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(" follow  "),
            Span::styled("↑↓ PgUp/PgDn ←→", Style::default().fg(Color::Cyan)),
            Span::raw(" scroll  "),
            Span::styled("c", Style::default().fg(Color::Cyan)),
            Span::raw(" clear  "),
            Span::styled(":", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw(" command"),
        ])
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn panel(title: &'static str, lines: Vec<Line<'static>>) -> Paragraph<'static> {
    Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(title, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)))
    )
}

fn usage_line(label: &str, pct: f64) -> Line<'static> {
    let pct = pct.clamp(0.0, 100.0);
    Line::from(vec![
        Span::raw(format!(" {label:<5}")),
        Span::styled(mini_bar(pct, 14), threshold_style(pct, true)),
        Span::styled(format!(" {pct:5.1}%"), threshold_style(pct, true)),
    ])
}

fn mini_bar(pct: f64, width: usize) -> String {
    let filled = ((pct.clamp(0.0, 100.0) / 100.0) * width as f64).round() as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(width.saturating_sub(filled)))
}

fn threshold_style(value: f64, high_is_bad: bool) -> Style {
    let effective = if high_is_bad { value } else { 100.0 - value };
    if effective >= 90.0 {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if effective >= 75.0 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Green)
    }
}

fn availability_style(available_pct: f64) -> Style {
    if available_pct <= 7.0 {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if available_pct <= 15.0 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Green)
    }
}


fn generation_state_style(state: &str) -> Style {
    match state {
        "Sampling" => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        "Preparing / loading" | "Queued" | "Waiting for API" => {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        }
        "API unavailable" => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        _ => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    }
}

fn generation_eta(generation: &GenerationMetrics) -> Option<Duration> {
    let current = generation.current_step?;
    let total = generation.progress_total.or(generation.steps)?;
    let started = generation.sampling_started_at.as_ref()?;
    if current == 0 || current >= total {
        return (current >= total).then_some(Duration::ZERO);
    }
    let elapsed = started.elapsed().as_secs_f64();
    let seconds_per_step = elapsed / current as f64;
    Some(Duration::from_secs_f64(
        seconds_per_step * total.saturating_sub(current) as f64,
    ))
}

fn format_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn short_prompt_id(prompt_id: &str) -> String {
    prompt_id.chars().take(8).collect()
}

fn truncate_middle(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_owned();
    }
    if max_chars <= 3 {
        return text.chars().take(max_chars).collect();
    }
    let left = (max_chars - 1) / 2;
    let right = max_chars - left - 1;
    let start = text.chars().take(left).collect::<String>();
    let end = text
        .chars()
        .rev()
        .take(right)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{start}…{end}")
}

fn format_rate(bytes_per_sec: Option<f64>) -> String {
    bytes_per_sec.map_or_else(
        || "N/A".to_owned(),
        |value| format!("{}/s", human_bytes(value.max(0.0) as u64)),
    )
}

fn format_optional_rate(value: Option<f64>) -> String {
    value.map_or_else(|| "N/A".to_owned(), |value| format!("{value:.1}"))
}

fn format_optional_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "N/A".to_owned(), |value| value.to_string())
}

fn spawn_log_reader<R: Read + Send + 'static>(mut reader: R, kind: StreamKind, tx: Sender<LogEvent>) {
    thread::spawn(move || {
        let mut chunk = [0_u8; 4096];
        let mut pending = Vec::<u8>::new();
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => {
                    emit_log_fragment(&tx, kind, &mut pending);
                    break;
                }
                Ok(read) => {
                    for byte in &chunk[..read] {
                        if *byte == b'\n' || *byte == b'\r' {
                            emit_log_fragment(&tx, kind, &mut pending);
                        } else {
                            pending.push(*byte);
                        }
                    }
                }
                Err(error) => {
                    let _ = tx.send(LogEvent {
                        kind: StreamKind::Warning,
                        text: format!("log stream read error: {error}"),
                        progress: None,
                    });
                    break;
                }
            }
        }
    });
}

fn emit_log_fragment(tx: &Sender<LogEvent>, kind: StreamKind, pending: &mut Vec<u8>) {
    if pending.is_empty() {
        return;
    }
    let raw = String::from_utf8_lossy(pending).into_owned();
    pending.clear();
    let text = strip_ansi(&raw);
    if text.trim().is_empty() {
        return;
    }
    let progress = parse_tqdm_progress(&text);
    let _ = tx.send(LogEvent {
        kind,
        text,
        progress,
    });
}

fn parse_tqdm_progress(line: &str) -> Option<ProgressUpdate> {
    if !(line.contains("it/s") || line.contains("s/it")) {
        return None;
    }
    let percent_marker = line.find("%|")?;
    let after_marker = &line[percent_marker + 2..];
    let bar_end = after_marker.find('|')?;
    let after_bar = &after_marker[bar_end + 1..];
    for token in after_bar.split_whitespace() {
        let token = token.trim_matches(|character: char| !character.is_ascii_digit() && character != '/');
        let Some((current, total)) = token.split_once('/') else {
            continue;
        };
        let current = current.parse::<u64>().ok()?;
        let total = total.parse::<u64>().ok()?;
        if total > 0 && current <= total {
            return Some(ProgressUpdate { current, total });
        }
    }
    None
}


fn collect_generation_snapshot(config: &Config) -> GenerationSnapshot {
    let mut snapshot = GenerationSnapshot::default();
    let queue = match http_get_json(&config.api_host, config.api_port, "/queue") {
        Ok(value) => value,
        Err(error) => {
            snapshot.api_error = Some(error.to_string());
            return snapshot;
        }
    };

    snapshot.api_connected = true;
    let running = queue
        .get("queue_running")
        .and_then(Value::as_array)
        .map(|items| items.as_slice())
        .unwrap_or(&[]);
    let pending = queue
        .get("queue_pending")
        .and_then(Value::as_array)
        .map(|items| items.as_slice())
        .unwrap_or(&[]);
    snapshot.running = running.len();
    snapshot.pending = pending.len();

    let Some(item) = running.first() else {
        return snapshot;
    };

    if let Some(parts) = item.as_array() {
        snapshot.prompt_number = parts.first().and_then(json_scalar_to_string);
        snapshot.prompt_id = parts.get(1).and_then(json_scalar_to_string);
        snapshot.output_node_count = parts.get(4).and_then(Value::as_array).map(Vec::len);
        if let Some(prompt) = parts.get(2) {
            extract_generation_settings(prompt, &mut snapshot);
        }
    } else if let Some(object) = item.as_object() {
        snapshot.prompt_number = object.get("number").and_then(json_scalar_to_string);
        snapshot.prompt_id = object.get("prompt_id").and_then(json_scalar_to_string);
        snapshot.output_node_count = object
            .get("outputs_to_execute")
            .and_then(Value::as_array)
            .map(Vec::len);
        if let Some(prompt) = object.get("prompt") {
            extract_generation_settings(prompt, &mut snapshot);
        }
    }

    snapshot
}

fn http_get_json(host: &str, port: u16, path: &str) -> Result<Value> {
    let address = format!("{host}:{port}")
        .to_socket_addrs()
        .with_context(|| format!("resolving ComfyUI API address {host}:{port}"))?
        .next()
        .ok_or_else(|| anyhow!("ComfyUI API address resolved to no sockets"))?;
    let timeout = Duration::from_millis(350);
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .with_context(|| format!("connecting to ComfyUI API at {host}:{port}"))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .context("sending ComfyUI API request")?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .context("reading ComfyUI API response")?;
    let response = String::from_utf8_lossy(&response);
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed HTTP response from ComfyUI"))?;
    let status = headers.lines().next().unwrap_or_default();
    if !status.contains(" 200 ") {
        bail!("ComfyUI API returned {status}");
    }
    serde_json::from_str(body).context("parsing ComfyUI queue JSON")
}

fn extract_generation_settings(prompt: &Value, snapshot: &mut GenerationSnapshot) {
    let Some(nodes) = prompt.as_object() else {
        return;
    };
    snapshot.node_count = Some(nodes.len());

    let mut sampler_priority = u8::MAX;
    let mut resolution_priority = u8::MAX;

    for node in nodes.values() {
        let Some(node_object) = node.as_object() else {
            continue;
        };
        let class_type = node_object
            .get("class_type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let class_lower = class_type.to_ascii_lowercase();
        let Some(inputs) = node_object.get("inputs").and_then(Value::as_object) else {
            continue;
        };

        if class_lower.contains("sampler") {
            let priority = if class_lower.contains("ksampler") { 0 } else { 1 };
            if priority < sampler_priority {
                sampler_priority = priority;
                snapshot.steps = json_input_u64(inputs, &["steps"]);
                snapshot.cfg = json_input_f64(inputs, &["cfg"]);
                snapshot.sampler = json_input_string(inputs, &["sampler_name", "sampler"]);
                snapshot.scheduler = json_input_string(inputs, &["scheduler"]);
                snapshot.seed = json_input_string(inputs, &["seed", "noise_seed"]);
                snapshot.denoise = json_input_f64(inputs, &["denoise"]);
            }
        }

        if snapshot.steps.is_none()
            && (class_lower.contains("scheduler") || class_lower.contains("sampler"))
        {
            snapshot.steps = json_input_u64(inputs, &["steps"]);
        }
        if snapshot.sampler.is_none() {
            snapshot.sampler = json_input_string(inputs, &["sampler_name"]);
        }
        if snapshot.scheduler.is_none() {
            snapshot.scheduler = json_input_string(inputs, &["scheduler"]);
        }
        if snapshot.cfg.is_none() {
            snapshot.cfg = json_input_f64(inputs, &["cfg"]);
        }
        if snapshot.seed.is_none() {
            snapshot.seed = json_input_string(inputs, &["noise_seed", "seed"]);
        }
        if snapshot.denoise.is_none() {
            snapshot.denoise = json_input_f64(inputs, &["denoise"]);
        }
        if snapshot.guidance.is_none() {
            snapshot.guidance = json_input_f64(inputs, &["guidance"]);
        }
        if snapshot.shift.is_none() {
            snapshot.shift = json_input_f64(inputs, &["shift"]);
        }

        if let (Some(width), Some(height)) = (
            json_input_u64(inputs, &["width"]),
            json_input_u64(inputs, &["height"]),
        ) {
            let priority = if class_lower.contains("latent") { 0 } else { 1 };
            if priority < resolution_priority {
                resolution_priority = priority;
                snapshot.width = Some(width);
                snapshot.height = Some(height);
                snapshot.batch_size = json_input_u64(inputs, &["batch_size", "batch"]);
            }
        }
        if snapshot.batch_size.is_none() {
            snapshot.batch_size = json_input_u64(inputs, &["batch_size"]);
        }

        if snapshot.model.is_none()
            && (class_lower.contains("loader")
                || class_lower.contains("checkpoint")
                || class_lower.contains("unet")
                || class_lower.contains("gguf"))
        {
            snapshot.model = json_input_string(
                inputs,
                &[
                    "unet_name",
                    "ckpt_name",
                    "model_name",
                    "diffusion_model_name",
                    "model_path",
                ],
            );
        }
        if snapshot.text_encoder.is_none()
            && (class_lower.contains("clip") || class_lower.contains("textencoder"))
        {
            snapshot.text_encoder = json_input_string(
                inputs,
                &["clip_name", "clip_name1", "text_encoder_name", "text_encoder"],
            );
        }
        if snapshot.vae.is_none() && class_lower.contains("vae") {
            snapshot.vae = json_input_string(inputs, &["vae_name"]);
        }
        if class_lower.contains("lora") {
            if let Some(name) = json_input_string(inputs, &["lora_name", "name"]) {
                let strength = json_input_f64(inputs, &["strength_model", "strength"])
                    .map(|value| format!(" @{value:.2}"))
                    .unwrap_or_default();
                snapshot.loras.push(format!("{name}{strength}"));
            }
        }
    }
}

fn json_input_u64(
    inputs: &serde_json::Map<String, Value>,
    keys: &[&str],
) -> Option<u64> {
    keys.iter().find_map(|key| inputs.get(*key).and_then(json_value_u64))
}

fn json_input_f64(
    inputs: &serde_json::Map<String, Value>,
    keys: &[&str],
) -> Option<f64> {
    keys.iter().find_map(|key| inputs.get(*key).and_then(json_value_f64))
}

fn json_input_string(
    inputs: &serde_json::Map<String, Value>,
    keys: &[&str],
) -> Option<String> {
    keys.iter()
        .find_map(|key| inputs.get(*key).and_then(json_scalar_to_string))
}

fn json_value_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|number| u64::try_from(number).ok()))
        .or_else(|| value.as_f64().filter(|number| *number >= 0.0).map(|number| number as u64))
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

fn json_value_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

fn json_scalar_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(boolean) => Some(boolean.to_string()),
        _ => None,
    }
}

fn collect_memory() -> MemoryMetrics {
    let values = parse_meminfo();
    let total = values.get("MemTotal").copied().unwrap_or(0);
    let available = values.get("MemAvailable").copied().unwrap_or(0);
    let swap_total = values.get("SwapTotal").copied().unwrap_or(0);
    let swap_free = values.get("SwapFree").copied().unwrap_or(0);
    let cached = values
        .get("Cached")
        .copied()
        .unwrap_or(0)
        .saturating_add(values.get("SReclaimable").copied().unwrap_or(0));
    let (pressure_some_avg10, pressure_full_avg10) = read_pressure("/proc/pressure/memory");
    let (io_pressure_some_avg10, io_pressure_full_avg10) = read_pressure("/proc/pressure/io");
    MemoryMetrics {
        total,
        available,
        used: total.saturating_sub(available),
        cached,
        dirty: values.get("Dirty").copied().unwrap_or(0),
        writeback: values.get("Writeback").copied().unwrap_or(0),
        swap_total,
        swap_used: swap_total.saturating_sub(swap_free),
        swap_in_bytes_per_sec: None,
        swap_out_bytes_per_sec: None,
        major_faults_per_sec: None,
        pressure_some_avg10,
        pressure_full_avg10,
        io_pressure_some_avg10,
        io_pressure_full_avg10,
    }
}

fn read_pressure(path: &str) -> (Option<f64>, Option<f64>) {
    let Ok(text) = fs::read_to_string(path) else {
        return (None, None);
    };
    let mut some = None;
    let mut full = None;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let kind = fields.next().unwrap_or_default();
        let avg10 = fields
            .find_map(|field| field.strip_prefix("avg10="))
            .and_then(|value| value.parse::<f64>().ok());
        match kind {
            "some" => some = avg10,
            "full" => full = avg10,
            _ => {}
        }
    }
    (some, full)
}

fn read_vm_counters() -> Option<VmCounters> {
    let text = fs::read_to_string("/proc/vmstat").ok()?;
    let mut counters = VmCounters::default();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let key = fields.next()?;
        let value = fields.next().and_then(|value| value.parse::<u64>().ok()).unwrap_or(0);
        match key {
            "pswpin" => counters.swap_in_pages = value,
            "pswpout" => counters.swap_out_pages = value,
            "pgmajfault" => counters.major_faults = value,
            _ => {}
        }
    }
    Some(counters)
}

fn parse_meminfo() -> HashMap<String, u64> {
    let Ok(text) = fs::read_to_string("/proc/meminfo") else {
        return HashMap::new();
    };
    text.lines()
        .filter_map(|line| {
            let (key, rest) = line.split_once(':')?;
            let value_kib = rest.split_whitespace().next()?.parse::<u64>().ok()?;
            Some((key.to_owned(), value_kib.saturating_mul(1024)))
        })
        .collect()
}

fn read_cpu_times() -> Option<CpuTimes> {
    let text = fs::read_to_string("/proc/stat").ok()?;
    let line = text.lines().next()?;
    let mut fields = line.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    let values = fields.filter_map(|field| field.parse::<u64>().ok()).collect::<Vec<_>>();
    if values.len() < 4 {
        return None;
    }
    let idle = values[3].saturating_add(values.get(4).copied().unwrap_or(0));
    let total = values.iter().copied().sum();
    Some(CpuTimes { idle, total })
}

fn read_load_average() -> (Option<f64>, Option<f64>, Option<f64>) {
    let Ok(text) = fs::read_to_string("/proc/loadavg") else {
        return (None, None, None);
    };
    let mut fields = text.split_whitespace();
    (
        fields.next().and_then(|v| v.parse().ok()),
        fields.next().and_then(|v| v.parse().ok()),
        fields.next().and_then(|v| v.parse().ok()),
    )
}

fn read_average_cpu_frequency_mhz() -> Option<f64> {
    let base = Path::new("/sys/devices/system/cpu");
    let entries = fs::read_dir(base).ok()?;
    let mut total_khz = 0.0;
    let mut count = 0_u64;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("cpu") || !name[3..].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let path = entry.path().join("cpufreq/scaling_cur_freq");
        if let Some(value) = read_trimmed(path).and_then(|v| v.parse::<f64>().ok()) {
            total_khz += value;
            count += 1;
        }
    }

    (count > 0).then_some(total_khz / count as f64 / 1000.0)
}

fn read_cpu_temperature() -> Option<f64> {
    let base = Path::new("/sys/class/hwmon");
    let entries = fs::read_dir(base).ok()?;
    let mut preferred = Vec::new();
    let mut fallback = Vec::new();

    for entry in entries.flatten() {
        let hwmon = entry.path();
        let chip_name = read_trimmed(hwmon.join("name")).unwrap_or_default();
        let chip_is_cpu = matches!(chip_name.as_str(), "coretemp" | "k10temp" | "zenpower" | "cpu_thermal");
        let Ok(files) = fs::read_dir(&hwmon) else {
            continue;
        };
        for file in files.flatten() {
            let file_name = file.file_name().to_string_lossy().into_owned();
            if !file_name.starts_with("temp") || !file_name.ends_with("_input") {
                continue;
            }
            let Some(raw) = read_trimmed(file.path()).and_then(|v| v.parse::<f64>().ok()) else {
                continue;
            };
            let celsius = raw / 1000.0;
            if !(0.0..=125.0).contains(&celsius) {
                continue;
            }
            let stem = file_name.trim_end_matches("_input");
            let label = read_trimmed(hwmon.join(format!("{stem}_label"))).unwrap_or_default();
            let is_package = label.to_ascii_lowercase().contains("package")
                || label.to_ascii_lowercase().contains("cpu");
            if chip_is_cpu && is_package {
                preferred.push(celsius);
            } else if chip_is_cpu {
                fallback.push(celsius);
            }
        }
    }

    preferred
        .into_iter()
        .reduce(f64::max)
        .or_else(|| fallback.into_iter().reduce(f64::max))
}

fn collect_gpu(index: u32) -> GpuMetrics {
    let query = "name,temperature.gpu,utilization.gpu,memory.total,memory.used,memory.free,power.draw,power.limit,fan.speed,clocks.current.graphics,pstate";
    let output = Command::new("nvidia-smi")
        .arg(format!("--id={index}"))
        .arg(format!("--query-gpu={query}"))
        .arg("--format=csv,noheader,nounits")
        .output();

    let output = match output {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            return GpuMetrics {
                error: Some(String::from_utf8_lossy(&output.stderr).trim().to_owned()),
                ..Default::default()
            };
        }
        Err(error) => {
            return GpuMetrics {
                error: Some(error.to_string()),
                ..Default::default()
            };
        }
    };

    let line = String::from_utf8_lossy(&output.stdout);
    let fields = line.trim().split(',').map(str::trim).collect::<Vec<_>>();
    if fields.len() < 11 {
        return GpuMetrics {
            error: Some(format!("unexpected nvidia-smi output: {}", line.trim())),
            ..Default::default()
        };
    }

    GpuMetrics {
        available: true,
        name: fields[0].to_owned(),
        temperature_c: parse_optional_f64(fields[1]),
        utilization_pct: parse_optional_f64(fields[2]),
        vram_total: parse_optional_mib(fields[3]),
        vram_used: parse_optional_mib(fields[4]),
        vram_free: parse_optional_mib(fields[5]),
        power_draw_w: parse_optional_f64(fields[6]),
        power_limit_w: parse_optional_f64(fields[7]),
        fan_pct: parse_optional_f64(fields[8]),
        graphics_clock_mhz: parse_optional_f64(fields[9]),
        pstate: parse_optional_string(fields[10]),
        error: None,
    }
}

fn collect_scope(unit: &str) -> ScopeMetrics {
    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            unit,
            "--no-pager",
            "--property=ActiveState,SubState,MemoryCurrent,MemoryPeak,MemoryHigh,MemoryMax,MemorySwapCurrent,MemorySwapMax,CPUUsageNSec,TasksCurrent,IOReadBytes,IOWriteBytes,ControlGroup",
        ])
        .output();

    let Ok(output) = output else {
        return ScopeMetrics::default();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let values = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect::<HashMap<_, _>>();
    let memory_events = values
        .get("ControlGroup")
        .copied()
        .and_then(read_cgroup_memory_events)
        .unwrap_or_default();

    ScopeMetrics {
        active_state: values.get("ActiveState").copied().unwrap_or_default().to_owned(),
        sub_state: values.get("SubState").copied().unwrap_or_default().to_owned(),
        memory_current: values.get("MemoryCurrent").and_then(|v| parse_systemd_bytes(*v)),
        memory_peak: values.get("MemoryPeak").and_then(|v| parse_systemd_bytes(*v)),
        memory_high: values.get("MemoryHigh").and_then(|v| parse_systemd_bytes(*v)),
        memory_max: values.get("MemoryMax").and_then(|v| parse_systemd_bytes(*v)),
        memory_swap_current: values
            .get("MemorySwapCurrent")
            .and_then(|v| parse_systemd_bytes(*v)),
        memory_swap_max: values
            .get("MemorySwapMax")
            .and_then(|v| parse_systemd_bytes(*v)),
        cpu_usage_nsec: values.get("CPUUsageNSec").and_then(|v| parse_systemd_u64(*v)),
        cpu_usage_pct: None,
        tasks_current: values.get("TasksCurrent").and_then(|v| parse_systemd_u64(*v)),
        io_read_bytes: values.get("IOReadBytes").and_then(|v| parse_systemd_u64(*v)),
        io_write_bytes: values.get("IOWriteBytes").and_then(|v| parse_systemd_u64(*v)),
        memory_high_events: memory_events.get("high").copied(),
        memory_max_events: memory_events.get("max").copied(),
        oom_events: memory_events.get("oom").copied(),
        oom_kill_events: memory_events.get("oom_kill").copied(),
    }
}

fn read_cgroup_memory_events(control_group: &str) -> Option<HashMap<String, u64>> {
    if control_group.trim().is_empty() || control_group == "[not set]" {
        return None;
    }
    let path = Path::new("/sys/fs/cgroup")
        .join(control_group.trim_start_matches('/'))
        .join("memory.events");
    let text = fs::read_to_string(path).ok()?;
    Some(
        text.lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let key = fields.next()?.to_owned();
                let value = fields.next()?.parse::<u64>().ok()?;
                Some((key, value))
            })
            .collect(),
    )
}

fn collect_disk(path: &Path) -> DiskMetrics {
    let output = Command::new("df")
        .args(["-B1", "--output=size,used,avail,pcent,target"])
        .arg(path)
        .output();
    let Ok(output) = output else {
        return DiskMetrics::default();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(line) = text.lines().filter(|line| !line.trim().is_empty()).last() else {
        return DiskMetrics::default();
    };
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 5 {
        return DiskMetrics::default();
    }
    DiskMetrics {
        total: fields[0].parse().unwrap_or(0),
        used: fields[1].parse().unwrap_or(0),
        available: fields[2].parse().unwrap_or(0),
        used_pct: fields[3].trim_end_matches('%').parse().unwrap_or(0.0),
        mount: fields[4..].join(" "),
    }
}

fn collect_power() -> PowerMetrics {
    let base = Path::new("/sys/class/power_supply");
    let Ok(entries) = fs::read_dir(base) else {
        return PowerMetrics::default();
    };
    let mut result = PowerMetrics::default();

    for entry in entries.flatten() {
        let path = entry.path();
        let kind = read_trimmed(path.join("type")).unwrap_or_default();
        match kind.as_str() {
            "Mains" | "USB" | "USB_C" => {
                if let Some(value) = read_trimmed(path.join("online")) {
                    if value == "1" {
                        result.ac_online = Some(true);
                    } else if result.ac_online.is_none() {
                        result.ac_online = Some(false);
                    }
                }
            }
            "Battery" => {
                result.battery_pct = read_trimmed(path.join("capacity")).and_then(|v| v.parse().ok());
                result.battery_status = read_trimmed(path.join("status"));
            }
            _ => {}
        }
    }
    result
}

fn systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("running systemctl {}", args.join(" ")))?;
    if status.success() {
        Ok(())
    } else {
        bail!("systemctl {} exited with {status}", args.join(" "))
    }
}

fn scope_is_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", unit])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn command_available(command: &str) -> Result<()> {
    Command::new(command)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("required command not found: {command}"))?;
    Ok(())
}

fn format_optional_decimal(value: Option<f64>) -> String {
    value.map_or_else(|| "N/A".to_owned(), |value| format!("{value:.1}"))
}

fn parse_optional_f64(value: &str) -> Option<f64> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("n/a") || value.starts_with('[') {
        None
    } else {
        value.parse().ok()
    }
}

fn parse_optional_mib(value: &str) -> Option<u64> {
    parse_optional_f64(value).map(|mib| (mib * 1024.0 * 1024.0) as u64)
}

fn parse_optional_string(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("n/a") || value.starts_with('[') {
        None
    } else {
        Some(value.to_owned())
    }
}

fn parse_systemd_u64(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() || value == "[not set]" || value.eq_ignore_ascii_case("infinity") {
        None
    } else {
        value.parse().ok()
    }
}

fn parse_systemd_bytes(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty()
        || value == "[not set]"
        || value.eq_ignore_ascii_case("infinity")
        || value == "18446744073709551615"
    {
        None
    } else {
        value.parse().ok()
    }
}

fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok().map(|text| text.trim().to_owned())
}

fn config_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("comfytui/config.toml"));
    }
    Ok(home_dir()
        .ok_or_else(|| anyhow!("HOME is not set"))?
        .join(".config/comfytui/config.toml"))
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

fn percent(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        used as f64 / total as f64 * 100.0
    }
}

fn gib(value: u64) -> u64 {
    value.saturating_mul(1024 * 1024 * 1024)
}

fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
                continue;
            }
        }
        if ch == '\r' {
            continue;
        }
        output.push(if ch == '\t' { ' ' } else { ch });
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_sequences() {
        assert_eq!(strip_ansi("\u{1b}[31merror\u{1b}[0m"), "error");
    }

    #[test]
    fn formats_binary_units() {
        assert_eq!(human_bytes(1024), "1.00 KiB");
        assert_eq!(human_bytes(25 * 1024 * 1024 * 1024), "25.0 GiB");
    }

    #[test]
    fn parses_systemd_memory_values() {
        assert_eq!(parse_systemd_bytes("26843545600"), Some(26_843_545_600));
        assert_eq!(parse_systemd_bytes("infinity"), None);
        assert_eq!(parse_systemd_bytes("[not set]"), None);
    }




    #[test]
    fn parses_tqdm_sampling_progress() {
        let progress = parse_tqdm_progress(" 35%|███▌      | 7/20 [00:42<01:18, 6.01s/it]")
            .expect("progress should parse");
        assert_eq!(progress.current, 7);
        assert_eq!(progress.total, 20);
    }

    #[test]
    fn extracts_common_generation_settings() {
        let prompt = serde_json::json!({
            "1": {"class_type": "KSampler", "inputs": {
                "steps": 25, "cfg": 4.0, "sampler_name": "euler",
                "scheduler": "simple", "seed": 123456, "denoise": 1.0
            }},
            "2": {"class_type": "EmptyLatentImage", "inputs": {
                "width": 1024, "height": 1024, "batch_size": 1
            }},
            "3": {"class_type": "UNETLoader", "inputs": {
                "unet_name": "qwen-image-Q4_K_M.gguf"
            }},
            "4": {"class_type": "CLIPLoader", "inputs": {
                "clip_name": "qwen_2.5_vl_7b_fp8_scaled.safetensors"
            }},
            "5": {"class_type": "LoraLoader", "inputs": {
                "lora_name": "character.safetensors", "strength_model": 0.85
            }}
        });
        let mut snapshot = GenerationSnapshot::default();
        extract_generation_settings(&prompt, &mut snapshot);
        assert_eq!(snapshot.steps, Some(25));
        assert_eq!(snapshot.width, Some(1024));
        assert_eq!(snapshot.height, Some(1024));
        assert_eq!(snapshot.model.as_deref(), Some("qwen-image-Q4_K_M.gguf"));
        assert_eq!(snapshot.sampler.as_deref(), Some("euler"));
        assert_eq!(snapshot.loras, vec!["character.safetensors @0.85"]);
    }

    #[test]
    fn shortens_long_labels_in_the_middle() {
        assert_eq!(truncate_middle("abcdefghij", 7), "abc…hij");
    }

    #[test]
    fn default_config_enables_live_previews() {
        let config = Config::default();
        assert_eq!(
            config.comfy_args,
            vec!["main.py", "--lowvram", "--preview-method", "auto"]
        );
    }
}
