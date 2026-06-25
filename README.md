# ComfyWise

A Linux/Arch-focused Rust TUI that launches ComfyUI inside a protected systemd user scope, shows live ComfyUI logs, and keeps generation plus system diagnostics visible in one terminal.

The supplied defaults are tailored to:

- ComfyUI: `/media/daanh/Shared/comfy/ComfyUI`
- Python: `/home/daanh/.pyenv/versions/3.13.7/envs/venv13/bin/python`
- Arguments: `main.py --lowvram --preview-method auto`
- API: `127.0.0.1:8188`
- Memory guard: `MemoryHigh=23G`, `MemoryMax=25G`, `MemorySwapMax=3G`

There is deliberately no `--reserve-vram` setting. ComfyUI can use the available RTX 4060 VRAM, while the cgroup protects system RAM and swap. Live previews remain enabled with `--preview-method auto`.

## What the screen shows

### Generation panel

The upper-left panel reads the active job from ComfyUI's local `/queue` API and shows, where the workflow exposes them:

- Active/queued job count and shortened prompt ID
- Sampling progress, current step, elapsed time, and estimated remaining time
- Steps, sampler, scheduler, CFG, denoise, guidance, and model shift
- Seed, resolution, batch size, node count, and output-node count
- Diffusion/checkpoint model, text encoder, and active LoRAs
- Last generation duration after the queue becomes idle

Step progress is detected from ComfyUI's live tqdm output, including carriage-return progress lines that ordinary line readers often miss. The queue API supplies the workflow settings; custom nodes with unusual field names may show `?` rather than an invented value.

### Hardware and pressure diagnostics

- CPU usage, package temperature, average clock, load averages, ASUS profile, and power state
- NVIDIA GPU usage, temperature, P-state, graphics clock, VRAM, power, and fan readings
- RAM availability, cache, dirty/writeback memory, and swap usage
- Live swap-in/swap-out throughput and major page-fault rate
- Linux memory and I/O pressure-stall information (PSI)
- Free space on the filesystem containing ComfyUI
- Actual ComfyUI cgroup memory current/peak/high/max/swap values
- ComfyUI scope CPU use, task count, cumulative I/O, and cgroup memory events
- A global low-memory watchdog that stops ComfyUI before the whole desktop becomes unusable
- Warnings for low RAM, swap thrashing, pressure stalls, high temperatures, cgroup limits, and OOM kills

The right side streams stdout and stderr from ComfyUI. ANSI control sequences are removed so the log remains readable inside the TUI.

## Install on Arch Linux

Install Rust if necessary:

```bash
sudo pacman -S --needed rust
```

Extract/open this project and run:

```bash
./install.sh
```

The installer runs the unit tests, builds an optimized release binary, installs it at `~/.local/bin/comfywise`, preserves or creates the config, and runs preflight checks.

Then start ComfyUI with:

```bash
comfywise
```

## Commands

```text
comfywise                 Start ComfyUI and open the dashboard
comfywise --check         Validate paths, NVIDIA access and cgroup support
comfywise --print-config  Print the active config path
comfywise --version       Print the version
comfywise --help          Show usage
```

## Controls

```text
q / Ctrl-C       Stop ComfyUI and close ComfyWise
s                Stop ComfyUI but keep the dashboard open
r                Stop and restart ComfyUI in a fresh scope
k                Force-kill the complete ComfyUI scope
Space            Toggle live log following
Up / Down        Scroll logs by one line
PageUp/PageDown  Scroll logs by twenty lines
Home / End       Oldest logs / resume live following
Left / Right     Horizontal log scrolling
c                Clear the in-memory log buffer
```

## Configuration

The active file is:

```text
~/.config/comfywise/config.toml
```

Default content:

```toml
comfy_dir = "/media/daanh/Shared/comfy/ComfyUI"
python = "/home/daanh/.pyenv/versions/3.13.7/envs/venv13/bin/python"
comfy_args = ["main.py", "--lowvram", "--preview-method", "auto"]
memory_high = "23G"
memory_max = "25G"
memory_swap_max = "3G"
refresh_ms = 1000
max_log_lines = 20000
gpu_index = 0
api_host = "127.0.0.1"
api_port = 8188
auto_stop_on_low_memory = true
emergency_ram_floor_mib = 2048
emergency_consecutive_samples = 3
```

The Python virtual environment does not need to be activated. Calling the interpreter inside the venv directly is equivalent for this purpose and avoids shell activation and quoting failures.

### API address

The generation panel polls `http://api_host:api_port/queue`. Keep these values aligned with any `--listen` or `--port` arguments you add to `comfy_args`. The API is only read; ComfyWise does not submit, modify, or cancel prompts through it.

### Changing the memory ceiling

If a large Qwen model is killed while the desktop remains responsive, the guard worked. You can cautiously change `memory_max` to `26G`, although that leaves less safety margin for Hyprland, the browser, and the kernel. Keep `memory_high` below `memory_max`.

`memory_swap_max` limits swap consumed by the ComfyUI scope. Raising it can allow a generation to finish, but also makes long periods of swap thrashing more likely.

The watchdog uses Linux `MemAvailable`, not merely unused RAM. With the defaults, ComfyUI is stopped only after available system memory remains below 2 GiB for three consecutive one-second samples. This catches pressure caused by ComfyUI plus the browser and desktop together, even when ComfyUI itself has not yet reached `MemoryMax`.

## Failure behaviour

- Reaching `MemoryHigh` causes memory pressure and reclaim inside the scope.
- Reaching `MemoryMax` can invoke the cgroup OOM mechanism.
- Falling below the configured global RAM floor for the configured number of samples triggers an emergency stop.
- `OOMPolicy=kill` makes the ComfyUI workload die as a group instead of leaving related processes behind.
- Closing with `q` first sends `SIGINT`, waits five seconds, asks systemd to stop the scope, and only then uses `SIGKILL` if it is still active.
- The terminal screen and cursor are restored through a drop guard even when the application returns an error.

## Data sources

ComfyWise reads:

- ComfyUI `/queue` plus ComfyUI stdout/stderr progress output
- `/proc/stat`, `/proc/meminfo`, `/proc/vmstat`, `/proc/loadavg`
- `/proc/pressure/memory` and `/proc/pressure/io`
- CPU frequency and temperature under `/sys`
- ASUS platform profile under `/sys/firmware/acpi/platform_profile`
- battery/adapter data under `/sys/class/power_supply`
- NVIDIA metrics through `nvidia-smi`
- cgroup values and counters through `systemctl --user show` and `memory.events`
- storage space through `df`

## Uninstall

```bash
./uninstall.sh
```

The configuration file is intentionally retained.
