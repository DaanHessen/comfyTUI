import re

with open("src/main.rs", "r") as f:
    code = f.read()

# Add `colors` method to App
if "fn colors(&self) -> ThemeColors" not in code:
    code = code.replace("impl App {", "impl App {\n    fn colors(&self) -> ThemeColors {\n        self.config.theme.colors()\n    }\n", 1)

# Modify render functions to use app.colors()
code = code.replace("Color::Cyan", "app.colors().primary")
code = code.replace("Color::Blue", "app.colors().secondary")
code = code.replace("Color::DarkGray", "app.colors().border")
# For success, warning, error, info, we need context since some colors are hardcoded for sparklines, etc.
# Actually, the user asked for a polished UI. The default theme uses these colors:
# primary: Cyan, secondary: Blue, border: DarkGray, success: Green, warning: Yellow, error: Red, info: LightBlue

code = code.replace("Color::Green", "app.colors().success")
code = code.replace("Color::Yellow", "app.colors().warning")
code = code.replace("Color::Red", "app.colors().error")

# Fix functions that don't have `app: &App`
code = code.replace("fn render_cpu(frame: &mut Frame, area: Rect, metrics: &Metrics) {", "fn render_cpu(frame: &mut Frame, area: Rect, metrics: &Metrics, app: &App) {")
code = code.replace("fn render_gpu(frame: &mut Frame, area: Rect, metrics: &Metrics) {", "fn render_gpu(frame: &mut Frame, area: Rect, metrics: &Metrics, app: &App) {")
code = code.replace("fn render_memory(frame: &mut Frame, area: Rect, metrics: &Metrics) {", "fn render_memory(frame: &mut Frame, area: Rect, metrics: &Metrics, app: &App) {")

code = code.replace("render_cpu(frame, rows[0], &app.metrics);", "render_cpu(frame, rows[0], &app.metrics, app);")
code = code.replace("render_gpu(frame, rows[1], &app.metrics);", "render_gpu(frame, rows[1], &app.metrics, app);")
code = code.replace("render_memory(frame, rows[2], &app.metrics);", "render_memory(frame, rows[2], &app.metrics, app);")

code = code.replace("fn render_help_modal(frame: &mut Frame, area: Rect)", "fn render_help_modal(frame: &mut Frame, area: Rect, app: &App)")
code = code.replace("render_help_modal(frame, area);", "render_help_modal(frame, area, app);")

code = code.replace("fn render_boot_overlay(frame: &mut Frame, area: Rect, pct: f64)", "fn render_boot_overlay(frame: &mut Frame, area: Rect, pct: f64, app: &App)")
code = code.replace("render_boot_overlay(frame, area, pct);", "render_boot_overlay(frame, area, pct, app);")

# Also background and text? We can leave them for now since Ratatui defaults to terminal background.
# Wait, let's also remove `use ratatui::style::Color;` and instead just make sure we don't need it.

with open("src/main.rs", "w") as f:
    f.write(code)

