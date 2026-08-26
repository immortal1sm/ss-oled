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
    /// Row currently hovered as drop target during drag.
    drag_over: Option<usize>,
    /// Working text for the weather city-search field.
    city_query: String,
    /// Working text for the optional province/state disambiguation.
    province_filter: String,
    /// Whether the secret header field shows plain text.
    show_secret: bool,
    /// Status line for the UI.
    status: String,
}

impl App {
    fn load(path: PathBuf) -> Result<Self> {
        let text = std::fs::read_to_string(&path)?;
        let doc: toml::Value = toml::from_str(&text)?;
        let province_filter = doc
            .get("weather")
            .and_then(|w| w.get("province"))
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_string();

        let mut app = Self {
            config_path: path,
            doc,
            providers: Vec::new(),
            selected: None,
            drag_from: None,
            drag_over: None,
            city_query: String::new(),
            province_filter,
            show_secret: false,
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
                        v.get("enabled").and_then(|e| e.as_bool()).map(|_| {
                            let prio = v.get("priority").and_then(|p| p.as_integer()).unwrap_or(99);
                            (k.clone(), prio)
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Custom providers live under [providers.custom.<name>].
        if let Some(custom) = self
            .doc
            .get("providers")
            .and_then(|p| p.get("custom"))
            .and_then(|c| c.as_table())
        {
            for (name, v) in custom {
                if v.get("enabled").and_then(|e| e.as_bool()).is_some() {
                    let prio =
                        v.get("priority").and_then(|p| p.as_integer()).unwrap_or(99);
                    list.push((name.clone(), prio));
                }
            }
        }

        list.sort_by_key(|(_, prio)| *prio);
        // 'forecast' merged into 'weather' - hide obsolete section from GUI.
        self.providers = list
            .into_iter()
            .map(|(name, _)| name)
            .filter(|n| n != "forecast")
            .collect();
    }

    fn save(&mut self) -> Result<()> {
        // Purge the obsolete forecast section if present (merged into weather).
        if let Some(table) = self.doc.as_table_mut() {
            table.remove("forecast");
        }
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
                if let Some(section) = table.get_mut(name.as_str()).and_then(|v| v.as_table_mut()) {
                    section.insert("priority".into(), toml::Value::Integer((idx + 1) as i64));
                }
            }
        }
    }

    /// Get/set helpers for nested "section.key" paths.
    fn get_bool(&self, path: &str) -> bool {
        self.get_value(path)
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    fn set_bool(&mut self, path: &str, val: bool) {
        self.set_value(path, toml::Value::Boolean(val));
    }

    fn get_int(&self, path: &str) -> i64 {
        self.get_value(path)
            .and_then(|v| v.as_integer())
            .unwrap_or(0)
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
        let mut cur = &self.doc;
        for part in path.split('.') {
            cur = cur.get(part)?;
        }
        Some(cur)
    }

    fn get_value_owned(&self, path: &str) -> Option<toml::Value> {
        self.get_value(path).cloned()
    }

    /// Walk/create nested tables along `path` minus the last segment, then
    /// insert `val` under the final key. Works for any depth.
    fn set_value(&mut self, path: &str, val: toml::Value) {
        let parts: Vec<&str> = path.split('.').collect();
        if parts.len() < 2 {
            return;
        }
        let mut table = match self.doc.as_table_mut() {
            Some(t) => t,
            None => return,
        };
        for part in &parts[..parts.len() - 1] {
            if !table.contains_key(*part) {
                table.insert(part.to_string(), toml::Value::Table(Default::default()));
            }
            table = match table.get_mut(*part).and_then(|v| v.as_table_mut()) {
                Some(t) => t,
                None => return,
            };
        }
        table.insert(parts[parts.len() - 1].to_string(), val);
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let panel_h = ctx.screen_rect().height() - 70.0;

        // ---------- LEFT SIDEBAR: provider list ----------
        egui::SidePanel::left("providers_panel")
            .resizable(false)
            .default_width(240.0)
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
                            let is_selected = self.selected.as_deref() == Some(name.as_str())
                                || self.drag_from == Some(idx);

                            let row = ui.horizontal(|ui| {
                                let enabled_key = format!("{name}.enabled");
                                let mut enabled = self.get_bool(&enabled_key);
                                if ui.checkbox(&mut enabled, "").changed() {
                                    self.set_bool(&enabled_key, enabled);
                                }

                                let label = ui.selectable_label(is_selected, name.as_str());

                                // Transparent interaction layer over the whole
                                // row (checkbox excluded) handling select+drag.
                                let row_id = egui::Id::new(("provider_row", name));
                                let hitbox = ui.interact(
                                    label.rect.expand2(egui::vec2(60.0, 6.0)),
                                    row_id,
                                    egui::Sense::click_and_drag(),
                                );
                                if hitbox.clicked() {
                                    self.selected = Some(name.clone());
                                }
                                if hitbox.drag_started() {
                                    self.drag_from = Some(idx);
                                }
                            });

                            // Row rect for hover-swap hit testing.
                            let rect = row.response.rect;
                            row_rects.push((idx, rect));
                        }

                        // While dragging: highlight hovered row (no mutation).
                        // On release: move dragged item to hovered position.
                        if let Some(from) = self.drag_from {
                            let hover = ui.input(|i| i.pointer.hover_pos());
                            let released = ui.input(|i| i.pointer.any_released());

                            if released {
                                // Commit the move.
                                if let Some(to) = self.drag_over.take() {
                                    if to != from {
                                        let item = self.providers.remove(from);
                                        let to_adj = if to > from { to - 1 } else { to };
                                        self.providers.insert(to_adj, item);
                                        self.sync_priorities();
                                    }
                                }
                                self.drag_from = None;
                                self.drag_over = None;
                            } else if let Some(pos) = hover {
                                self.drag_over = row_rects
                                    .iter()
                                    .find(|(_, rect)| rect.contains(pos))
                                    .map(|(idx, _)| *idx);
                            }
                        }

                        // Paint insertion indicator on the hovered row.
                        if let (Some(_from), Some(over)) = (self.drag_from, self.drag_over) {
                            if let Some((_, rect)) = row_rects.iter().find(|(i, _)| *i == over) {
                                ui.painter().rect_stroke(
                                    *rect,
                                    2.0,
                                    egui::Stroke::new(1.5, egui::Color32::LIGHT_BLUE),
                                );
                            }
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

            // ---------- Geocoding search result delivery ----------
            // Polled globally so a search completes even if the user switches
            // provider tabs or collapses the weather section mid-request.
            {
                let mut pending = SEARCH.lock().unwrap().take();
                if let Some(rx) = &mut pending {
                    match rx.try_recv() {
                        Ok(Ok(payload)) => {
                            let mut it = payload.split('|');
                            let lat = it.next().unwrap_or("");
                            let lon = it.next().unwrap_or("");
                            let tz = it.next().unwrap_or("");
                            let name = it.next().unwrap_or("");
                            self.set_value(
                                "weather.latitude",
                                toml::Value::Float(lat.parse().unwrap_or(0.0)),
                            );
                            self.set_value(
                                "weather.longitude",
                                toml::Value::Float(lon.parse().unwrap_or(0.0)),
                            );
                            self.set_str("weather.timezone", tz);
                            self.set_str("weather.label", name);
                            self.status = format!("Found: {name}");
                            *SEARCH.lock().unwrap() = None;
                        }
                        Ok(Err(e)) => {
                            self.status = format!("Search failed: {e}");
                            *SEARCH.lock().unwrap() = None;
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {
                            // Still running: put the receiver back; surface
                            // which query variant is being tried right now.
                            *SEARCH.lock().unwrap() = pending.take();
                            let progress = SEARCH_PROGRESS.lock().unwrap().clone();
                            self.status = format!("Searching: '{progress}'…");
                        }
                        Err(_) => {
                            *SEARCH.lock().unwrap() = None;
                        }
                    }
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
    // Custom providers get their own editor.
    let is_custom = app
        .doc
        .get("providers")
        .and_then(|p| p.get("custom"))
        .and_then(|c| c.get(name))
        .is_some();
    if is_custom {
        custom_provider_editor(ui, app, name);
        return;
    }

    // Weather has its own two durations (today + forecast cycle); showing the
    // generic rotation dwell too would be three confusing numbers.
    if name != "weather" {
        let dwell_key = format!("interval.{name}");
        ui.horizontal(|ui| {
            ui.label("Duration (s):");
            let mut d = app.get_int(&dwell_key);
            if d == 0 {
                d = app.get_int("interval.refresh");
            }
            if ui
                .add(egui::DragValue::new(&mut d).clamp_range(1..=600))
                .changed()
            {
                ensure_interval_table(app).insert(name.to_string(), toml::Value::Integer(d));
            }
        });
    }

    match name {
        "mpris2" => {
            toggle(
                ui,
                app,
                "mpris2.event_focus",
                "Jump to screen on play/pause/track change",
            );
            toggle(
                ui,
                app,
                "mpris2.show_source_label",
                "Show source label (Firefox, Spotify…)",
            );
            toggle(ui, app, "mpris2.show_timer", "Show elapsed/total timer row");
        }
        "sysinfo" => {
            text_field(ui, app, "sysinfo.net_interface_name", "Network interface");
            text_field(ui, app, "sysinfo.sensor_name", "Temperature sensor");
            int_field(ui, app, "sysinfo.polling_interval", "Poll interval (ms)");
            int_field(ui, app, "sysinfo.temperature_max", "Temp max scale");
        }
        "image" => {
            ui.horizontal(|ui| {
                ui.label("GIF/logo file:");
                let mut s = app.get_str("image.path");
                if ui.text_edit_singleline(&mut s).lost_focus() && !s.is_empty() {
                    app.set_str("image.path", &s);
                }
                if ui.button("Browse…").clicked() {
                    if let Some(file) = rfd::FileDialog::new()
                        .add_filter("Images", &["gif", "png", "jpg", "jpeg", "webp", "bmp"])
                        .pick_file()
                    {
                        app.set_str("image.path", &file.to_string_lossy());
                    }
                }
            });
            toggle(ui, app, "image.dither", "Floyd–Steinberg dithering");
        }
        "weather" => {
            int_field(ui, app, "weather.duration", "Today duration (s)");
            int_field(
                ui,
                app,
                "weather.forecast_duration",
                "Forecast cycle total (s)",
            );
            // Optional province/state disambiguates same-named cities
            // (client-side filter on the geocoder's admin1/admin2 fields).
            ui.horizontal(|ui| {
                ui.label("Province/State (optional):");
                let mut p = app.province_filter.clone();
                if ui.text_edit_singleline(&mut p).changed() {
                    app.province_filter = p.clone();
                    app.set_str("weather.province", &p);
                }
            });

            // City search via Open-Meteo geocoding API. Fills lat/lon/tz.
            use std::sync::mpsc;
            let mut query = app.city_query.clone();
            let mut do_search = false;
            ui.horizontal(|ui| {
                ui.label("City:");
                let field = ui.text_edit_singleline(&mut query);
                // Keep the working text in App state every frame so keystrokes
                // survive repaints (the field re-inits from this each frame).
                app.city_query = query.clone();
                // Enter inside the field triggers search (standard UX).
                if field.lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    && !query.trim().is_empty()
                {
                    do_search = true;
                }
                do_search |= ui.button("Search").clicked();
            });
            if do_search {
                let q = query.trim().to_string();
                app.status = format!("Searching for '{q}'…");
                *SEARCH_PROGRESS.lock().unwrap() = q.clone();
                let province = app.province_filter.trim().to_lowercase();
                if !province.is_empty() {
                    app.set_str("weather.province", &province);
                }
                let (tx, rx) = mpsc::channel();
                // Overall budget for ALL fallback attempts combined.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
                std::thread::spawn(move || {
                    // Try the full query first; on zero results, progressively
                    // drop trailing words ("Science City of Munoz" -> "Science
                    // City of" -> "Science City"...). The geocoder's DB uses
                    // short canonical names ("Munoz"), so formal multi-word
                    // names often miss.
                    // Try the full query first; on zero results retry with
                    // right-truncated variants ("Science City of Munoz" ->
                    // "City of Munoz" -> "of Munoz" -> "Munoz"). The geocoder
                    // indexes short canonical names, and the LAST word of a
                    // formal name is usually the actual city.
                    // With a province filter we can afford a wide net:
                    // request 25 candidates and filter client-side on the
                    // geocoder's admin1 (region) / admin2 (province) / name.
                    let count = if province.is_empty() { 1 } else { 25 };

                    let result = (|| -> anyhow::Result<String> {
                        let words: Vec<&str> = q.split_whitespace().collect();
                        for start in 0..words.len() {
                            if std::time::Instant::now() >= deadline {
                                anyhow::bail!("timed out - try a shorter city name");
                            }
                            let name = words[start..].join(" ");
                            *SEARCH_PROGRESS.lock().unwrap() = name.clone();
                            let url = format!(
                                    "https://geocoding-api.open-meteo.com/v1/search?name={}&count={count}&language=en&format=json",
                                    urlencode(&name)
                                );
                            let body = ureq::get(&url)
                                .timeout(std::time::Duration::from_secs(5))
                                .call()?
                                .into_string()?;
                            let v: serde_json::Value = serde_json::from_str(&body)?;

                            let candidates = v["results"].as_array().cloned().unwrap_or_default();

                            // Province-filtered pick first (case-insensitive
                            // substring against admin1/admin2/name).
                            if !province.is_empty() {
                                for hit in &candidates {
                                    let matches_prov =
                                        |s: &str| s.to_lowercase().contains(&province);
                                    let hit_match = hit
                                        .get("admin1")
                                        .and_then(|a| a.as_str())
                                        .map(matches_prov)
                                        .unwrap_or(false)
                                        || hit
                                            .get("admin2")
                                            .and_then(|a| a.as_str())
                                            .map(matches_prov)
                                            .unwrap_or(false);
                                    let name_match = hit
                                        .get("name")
                                        .and_then(|n| n.as_str())
                                        .map(matches_prov)
                                        .unwrap_or(false);
                                    if hit_match || name_match {
                                        return Ok(format!(
                                            "{}|{}|{}|{}",
                                            hit["latitude"].as_f64().unwrap_or(0.0),
                                            hit["longitude"].as_f64().unwrap_or(0.0),
                                            hit["timezone"].as_str().unwrap_or("auto"),
                                            hit["name"].as_str().unwrap_or(""),
                                        ));
                                    }
                                }
                                // No province match for this variant; try the
                                // next suffix before giving up.
                                continue;
                            }

                            // Unfiltered: first hit wins.
                            if let Some(hit) = candidates.first() {
                                return Ok(format!(
                                    "{}|{}|{}|{}",
                                    hit["latitude"].as_f64().unwrap_or(0.0),
                                    hit["longitude"].as_f64().unwrap_or(0.0),
                                    hit["timezone"].as_str().unwrap_or("auto"),
                                    hit["name"].as_str().unwrap_or(""),
                                ));
                            }
                        }
                        anyhow::bail!("no results - try a shorter city name")
                    })();
                    result
                });
                *SEARCH.lock().unwrap() = Some(rx);
            }

            float_field(ui, app, "weather.latitude", "Latitude");
            float_field(ui, app, "weather.longitude", "Longitude");
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

fn float_field(ui: &mut egui::Ui, app: &mut App, key: &str, label: &str) {
    ui.horizontal(|ui| {
        ui.label(format!("{label}:"));
        // Display works for both Float and legacy String values; edits write
        // back as proper floats.
        let mut display = match app.get_value(key) {
            Some(toml::Value::Float(f)) => f.to_string(),
            Some(toml::Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        let response = ui.text_edit_singleline(&mut display);
        if response.changed() || response.lost_focus() {
            if let Ok(v) = display.trim().parse::<f64>() {
                app.set_value(key, toml::Value::Float(v));
            }
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


/// Editor for [providers.custom.<name>] sections.
fn custom_provider_editor(ui: &mut egui::Ui, app: &mut App, name: &str) {
    let base = format!("providers.custom.{name}");

    ui.horizontal(|ui| {
        ui.label(format!("{name} —"));
        ui.colored_label(egui::Color32::LIGHT_BLUE, "custom JSON-API screen");
    });
    ui.add_space(4.0);

    toggle(ui, app, &format!("{base}.enabled"), "Enabled");

    ui.horizontal(|ui| {
        ui.label("Duration (s):");
        let mut d = app.get_int(&format!("interval.{name}"));
        if d == 0 {
            d = app.get_int("interval.refresh");
        }
        if ui
            .add(egui::DragValue::new(&mut d).clamp_range(1..=600))
            .changed()
        {
            ensure_interval_table(app).insert(name.to_string(), toml::Value::Integer(d));
        }

        ui.label("Poll (s):");
        let mut p = app.get_int(&format!("{base}.poll"));
        if p == 0 {
            p = 300;
        }
        if ui
            .add(egui::DragValue::new(&mut p).clamp_range(10..=86400))
            .changed()
        {
            app.set_int(&format!("{base}.poll"), p);
        }
    });

    ui.add_space(6.0);
    ui.label("API endpoint:");
    text_field_multiline_ok(app, ui, &format!("{base}.source"));

    ui.add_space(6.0);
    ui.label("Header (optional, ${ENV_VAR} expanded):");
    // Masked input for secrets.
    ui.horizontal(|ui| {
        let path = format!("{base}.header");
        let mut s = app.get_str(&path);
        let masked = !s.is_empty() && !app.show_secret;
        let response = ui.add(egui::TextEdit::singleline(&mut s).password(masked));
        if response.changed() {
            app.set_str(&path, &s);
        }
        if ui
            .toggle_value(&mut app.show_secret, "👁")
            .changed()
        {
            // toggling just re-renders
        }
    });

    ui.add_space(6.0);
    ui.separator();
    ui.label("Fields — JSON path : label");
    edit_fields_table(ui, app, &base);
}

fn text_field_multiline_ok(app: &mut App, ui: &mut egui::Ui, path: &str) {
    ui.horizontal(|ui| {
        let mut s = app.get_str(path);
        let response = ui.add(
            egui::TextEdit::singleline(&mut s)
                .desired_width(f32::INFINITY),
        );
        if response.changed() || response.lost_focus() {
            app.set_str(path, &s);
        }
    });
}

fn edit_fields_table(ui: &mut egui::Ui, app: &mut App, base: &str) {
    let fields_path = format!("{base}.fields");
    // Read current entries as strings.
    let mut rows: Vec<(String, String)> = Vec::new();
    if let Some(toml::Value::Array(arr)) = app.get_value_owned(&fields_path) {
        for v in arr {
            if let Some(s) = v.as_str() {
                let (path_part, label_part) = match s.split_once(':') {
                    Some((p, l)) => (p.trim().to_string(), l.trim().to_string()),
                    None => (s.trim().to_string(), String::new()),
                };
                rows.push((path_part, label_part));
            }
        }
    }

    let mut changed = false;
    let mut remove_idx: Option<usize> = None;

    egui::Grid::new(("fields_grid", base)).show(ui, |ui| {
        for (i, (path_part, label_part)) in rows.iter_mut().enumerate() {
            let p_resp = ui.add(
                egui::TextEdit::singleline(path_part)
                    .hint_text("json.path[0].key")
                    .desired_width(220.0),
            );
            let l_resp = ui.add(
                egui::TextEdit::singleline(label_part)
                    .hint_text("Label")
                    .desired_width(120.0),
            );
            if p_resp.changed() || l_resp.changed() {
                changed = true;
            }
            if ui.button("✕").clicked() {
                remove_idx = Some(i);
            }
            ui.end_row();
        }
        if ui.button("+ Add field").clicked() {
            rows.push((String::new(), String::new()));
            changed = true;
        }
        ui.end_row();
    });

    if let Some(idx) = remove_idx {
        rows.remove(idx);
        changed = true;
    }

    if changed {
        let serialized: Vec<toml::Value> = rows
            .iter()
            .filter(|(p, _)| !p.trim().is_empty())
            .map(|(p, l)| {
                if l.trim().is_empty() {
                    toml::Value::String(p.clone())
                } else {
                    toml::Value::String(format!("{}: {}", p.trim(), l.trim()))
                }
            })
            .collect();
        app.set_value(&fields_path, toml::Value::Array(serialized));
    }
}


/// Percent-encode a string for use as a URL query value.
fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Geocoding search state: weather tab spawns, global poll delivers.
static SEARCH: std::sync::Mutex<Option<std::sync::mpsc::Receiver<Result<String, String>>>> =
    std::sync::Mutex::new(None);

/// Live progress text from the running search thread (current query variant).
static SEARCH_PROGRESS: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_config_path);

    let app = App::load(path)?;
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 560.0])
            .with_min_inner_size([700.0, 480.0])
            .with_title("ss-oled settings"),
        ..Default::default()
    };
    eframe::run_native("ss-oled settings", native, Box::new(|_cc| Box::new(app)))
        .map_err(|e| anyhow::anyhow!("eframe error: {e}"))
}
