//! ss-oled configuration window.
//!
//! Schema-driven editor for settings.toml: General tab (rotation order via
//! up/down buttons + enable checkboxes, dwell times, shared location) and
//! one tab per provider rendering its declared fields generically.
//!
//! Button semantics:
//!   Revert — discard unsaved edits, reload from disk
//!   Save   — write settings.toml only (takes effect on next restart)
//!   Apply  — write file AND restart the daemon

use anyhow::Result;
use std::path::PathBuf;

// ---------- settings.toml model ----------
// We deserialize into a permissive Value tree so unknown keys survive
// round-trips (critical: never drop a key we don't understand).

fn default_config_path() -> PathBuf {
    dirs_or_home().join(".config/apex-tux/settings.toml")
}

fn dirs_or_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

struct App {
    config_path: PathBuf,
    /// Parsed TOML document being edited.
    doc: toml::Value,
    /// Provider names in current display order (from priority ints).
    providers: Vec<String>,
    /// Currently selected provider (drives the right-hand settings panel).
    selected: Option<String>,
    /// Row being dragged (index), persists across frames while held.
    drag_from: Option<usize>,
    /// Status line for the UI.
    status: String,
}

impl App {
    fn load(path: PathBuf) -> Result<Self> {
        let text = std::fs::read_to_string(&path)?;
        let doc: toml::Value = toml::from_str(&text)?;
        let mut app = Self {
            config_path: path,
            doc,
            providers: Vec::new(),
            selected: None,
            drag_from: None,
            status: "Loaded".into(),
        };
        app.refresh_provider_list();
        eprintln!(
            "apex-gui: loaded {} (providers: {:?})",
            app.config_path.display(),
            app.providers
        );
        Ok(app)
    }

    /// Collect provider sections in priority order.
    fn refresh_provider_list(&mut self) {
        let mut list: Vec<(String, i64)> = self
            .doc
            .as_table()
            .map(|t| {
                t.iter()
                    .filter_map(|(k, v)| {
                        // A provider section has an `enabled` key.
                        v.get("enabled")
                            .and_then(|e| e.as_bool())
                            .map(|_| {
                                let prio = v
                                    .get("priority")
                                    .and_then(|p| p.as_integer())
                                    .unwrap_or(99);
                                (k.clone(), prio)
                            })
                    })
                    .collect()
            })
            .unwrap_or_default();
        list.sort_by_key(|(_, prio)| *prio);
        self.providers = list.into_iter().map(|(name, _)| name).collect();
    }

    fn save(&mut self) -> Result<()> {
        let serialized = toml::to_string_pretty(&self.doc)?;
        std::fs::write(&self.config_path, serialized)?;
        self.status = "Saved to disk".into();
        Ok(())
    }

    fn apply(&mut self) {
        match self.save() {
            Ok(()) => match std::process::Command::new("systemctl")
                .args(["--user", "restart", "apex-tux"])
                .status()
            {
                Ok(_) => self.status = "Applied & daemon restarted".into(),
                Err(e) => self.status = format!("Saved but restart failed: {e}"),
            },
            Err(e) => self.status = format!("Save failed: {e}"),
        }
    }

    fn revert(&mut self) {
        match App::load(self.config_path.clone()) {
            Ok(mut fresh) => {
                fresh.status = "Reverted".into();
                *self = fresh;
            }
            Err(e) => self.status = format!("Revert failed: {e}"),
        }
    }

    /// Rewrite priorities from the provider list order.
    fn sync_priorities(&mut self) {
        if let Some(table) = self.doc.as_table_mut() {
            for (idx, name) in self.providers.iter().enumerate() {
                if let Some(section) = table.get_mut(name.as_str()).and_then(|v| v.as_table_mut())
                {
                    section.insert(
                        "priority".into(),
                        toml::Value::Integer((idx + 1) as i64),
                    );
                }
            }
        }
    }

    /// Get/set helpers for nested "section.key" paths.
    fn get_bool(&self, path: &str) -> bool {
        self.get_value(path).and_then(|v| v.as_bool()).unwrap_or(false)
    }

    fn set_bool(&mut self, path: &str, val: bool) {
        self.set_value(path, toml::Value::Boolean(val));
    }

    fn get_int(&self, path: &str) -> i64 {
        self.get_value(path).and_then(|v| v.as_integer()).unwrap_or(0)
    }

    fn set_int(&mut self, path: &str, val: i64) {
        self.set_value(path, toml::Value::Integer(val));
    }

    fn get_str(&self, path: &str) -> String {
        self.get_value(path)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    fn set_str(&mut self, path: &str, val: &str) {
        self.set_value(path, toml::Value::String(val.to_string()));
    }

    fn get_value(&self, path: &str) -> Option<&toml::Value> {
        let mut parts = path.split('.');
        let section = parts.next()?;
        let key = parts.next()?;
        self.doc.get(section).and_then(|sec| sec.get(key))
    }

    fn set_value(&mut self, path: &str, val: toml::Value) {
        let Some((section, key)) = path.split_once('.') else {
            return;
        };
        if let Some(table) = self.doc.as_table_mut() {
            if let Some(sec) = table.get_mut(section).and_then(|v| v.as_table_mut()) {
                sec.insert(key.to_string(), val);
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let panel_h = ctx.screen_rect().height() - 70.0;

        // ---------- LEFT SIDEBAR: provider list ----------
        egui::SidePanel::left("providers_panel")
            .resizable(false)
            .default_width(230.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.heading("Providers");
                ui.label("Drag up/down to reorder");
                ui.add_space(6.0);

                egui::ScrollArea::vertical()
                    .id_source("provider_list")
                    .max_height(panel_h)
                    .show(ui, |ui| {
                        let mut swap: Option<(usize, usize)> = None;
                        let mut row_rects: Vec<(usize, egui::Rect)> = Vec::new();

                        for (idx, name) in self.providers.clone().iter().enumerate() {
                            let is_selected = self.selected.as_deref() == Some(name.as_str());
                            ui.horizontal(|ui| {
                                let enabled_key = format!("{name}.enabled");
                                let mut enabled = self.get_bool(&enabled_key);
                                if ui.checkbox(&mut enabled, "").changed() {
                                    self.set_bool(&enabled_key, enabled);
                                }
                                let label = ui.selectable_label(is_selected, name.as_str());
                                if label.clicked() {
                                    self.selected = Some(name.clone());
                                }
                            });

                            // Record row rect AFTER drawing for drag hit-testing
                            // next frame (store last-known rects).
                            let rect = ui.min_rect();
                            row_rects.push((idx, rect));

                            // Start drag when pointer presses inside a row.
                            let interaction = ui.interact(
                                rect,
                                egui::Id::new(("row", idx)),
                                egui::Sense::click_and_drag(),
                            );
                            if interaction.drag_started() {
                                self.drag_from = Some(idx);
                            }
                        }

                        // While dragging, entering another row swaps live.
                        if let Some(from) = self.drag_from {
                            let hover = ui.input(|i| i.pointer.hover_pos());
                            let released = ui.input(|i| i.pointer.any_released());
                            if released {
                                self.drag_from = None;
                            } else if let Some(pos) = hover {
                                for (other, rect) in &row_rects {
                                    if *other != from && rect.contains(pos) {
                                        swap = Some((from, *other));
                                        break;
                                    }
                                }
                            }
                        }

                        if let Some((from, to)) = swap {
                            let item = self.providers.remove(from);
                            self.providers.insert(to, item);
                            self.drag_from = Some(to); // follow the dragged item
                            self.sync_priorities();
                        }
                    });
            });

        // ---------- CENTER: selected provider settings ----------
        egui::CentralPanel::default().show(ctx, |ui| {
            match self.selected.clone() {
                Some(name) => {
                    ui.heading(name.clone());
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .id_source("settings_scroll")
                        .max_height(panel_h)
                        .show(ui, |ui| {
                            provider_section(ui, self, &name);
                        });
                }
                None => {
                    ui.add_space(20.0);
                    ui.heading("ss-oled settings");
                    ui.label("Select a provider on the left to edit its configuration.");
                    ui.add_space(8.0);
                    ui.label("Changes write to settings.toml.");
                }
            }

            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Revert").clicked() {
                        self.revert();
                    }
                    if ui.button("Save").clicked() {
                        let _ = self.save();
                    }
                    if ui.button("Apply").clicked() {
                        self.apply();
                    }
                    ui.label(&self.status);
                });
            });
        });
    }
}


fn provider_section(ui: &mut egui::Ui, app: &mut App, name: &str) {
    let dwell_key = format!("interval.{name}");
    ui.horizontal(|ui| {
        ui.label("Duration (s):");
        let mut d = app.get_int(&dwell_key);
        if d == 0 {
            d = app.get_int("interval.refresh");
        }
        if ui.add(egui::DragValue::new(&mut d).clamp_range(1..=600)).changed() {
            // Write into interval.<name> creating the table if needed.
            ensure_interval_table(app).insert(name.to_string(), toml::Value::Integer(d));
        }
    });

    match name {
        "mpris2" => {
            toggle(ui, app, "mpris2.event_focus", "Jump to screen on play/pause/track change");
            toggle(ui, app, "mpris2.show_source_label", "Show source label (Firefox, Spotify…)");
            toggle(ui, app, "mpris2.show_timer", "Show elapsed/total timer row");
        }
        "sysinfo" => {
            text_field(ui, app, "sysinfo.net_interface_name", "Network interface");
            text_field(ui, app, "sysinfo.sensor_name", "Temperature sensor");
            int_field(ui, app, "sysinfo.polling_interval", "Poll interval (ms)");
            int_field(ui, app, "sysinfo.temperature_max", "Temp max scale");
        }
        "image" => {
            text_field(ui, app, "image.path", "GIF/logo file");
            toggle(ui, app, "image.dither", "Floyd–Steinberg dithering");
            ui.label("(preview coming in a future release)");
        }
        "weather" => {
            text_field(ui, app, "weather.latitude", "Latitude");
            text_field(ui, app, "weather.longitude", "Longitude");
            text_field(ui, app, "weather.timezone", "Timezone");
            ui.horizontal(|ui| {
                ui.label("Units:");
                let cur = app.get_str("weather.units");
                if ui.radio(cur != "imperial", "°C").clicked() {
                    app.set_str("weather.units", "metric");
                }
                if ui.radio(cur == "imperial", "°F").clicked() {
                    app.set_str("weather.units", "imperial");
                }
            });
        }
        "clock" => {
            toggle(ui, app, "clock.twelve_hour", "12-hour (AM/PM) format");
        }
        _ => {
            ui.label("(no extra options)");
        }
    }
}

// Widget helpers ------------------------------------------------------------

fn toggle(ui: &mut egui::Ui, app: &mut App, key: &str, label: &str) {
    let mut val = app.get_bool(key);
    if ui.checkbox(&mut val, label).changed() {
        app.set_bool(key, val);
    }
}

fn text_field(ui: &mut egui::Ui, app: &mut App, key: &str, label: &str) {
    ui.horizontal(|ui| {
        ui.label(format!("{label}:"));
        let mut s = app.get_str(key);
        if ui.text_edit_singleline(&mut s).lost_focus() && !s.is_empty() {
            app.set_str(key, &s);
        }
    });
}

fn int_field(ui: &mut egui::Ui, app: &mut App, key: &str, label: &str) {
    ui.horizontal(|ui| {
        ui.label(format!("{label}:"));
        let mut v = app.get_int(key);
        if ui.add(egui::DragValue::new(&mut v)).changed() {
            app.set_int(key, v);
        }
    });
}

/// Ensure `interval` table exists, returning mutable access.
fn ensure_interval_table(app: &mut App) -> &mut toml::value::Table {
    let doc = &mut app.doc;
    let root = doc.as_table_mut().expect("root is always a table");
    if !root.contains_key("interval") {
        root.insert("interval".into(), toml::Value::Table(Default::default()));
    }
    root.get_mut("interval")
        .and_then(|v| v.as_table_mut())
        .expect("interval is a table")
}

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_config_path);

    let app = App::load(path)?;
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 480.0])
            .with_title("ss-oled settings"),
        ..Default::default()
    };
    eframe::run_native("ss-oled settings", native, Box::new(|_cc| Box::new(app)))
        .map_err(|e| anyhow::anyhow!("eframe error: {e}"))
}
