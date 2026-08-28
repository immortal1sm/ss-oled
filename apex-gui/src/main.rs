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
    /// Live preview of the last API test response (pretty JSON).
    api_preview: Option<String>,
    /// Suggested (path, label) pairs generated from the last API response.
    api_suggested: Option<Vec<(String, String)>>,
    /// Receiver for the in-flight API test.
    api_test: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    /// Field-editor drag state (persists across frames while dragging).
    field_drag_from: Option<usize>,
    field_drag_over: Option<usize>,
    /// Whether a drag is currently active (any handle being held).
    field_drag_active: bool,
    /// Set true when the user releases the handle; commit happens on the
    /// following frame (after the closure finishes) so row_rects are stable.
    field_drag_pending_commit: bool,
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
            api_preview: None,
            api_suggested: None,
            api_test: None,
            field_drag_from: None,
            field_drag_over: None,
            field_drag_active: false,
            field_drag_pending_commit: false,
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
                    let prio = v.get("priority").and_then(|p| p.as_integer()).unwrap_or(99);
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

    /// Rewrite priorities from the provider list order so the new order
    /// survives save + daemon restart. Built-in providers have their
    /// section at the top level; custom providers nest under
    /// `[providers.custom.<name>]`, so we update both paths.
    fn sync_priorities(&mut self) {
        let names: Vec<String> = self.providers.clone();
        if let Some(table) = self.doc.as_table_mut() {
            for (idx, name) in names.iter().enumerate() {
                let prio = toml::Value::Integer((idx + 1) as i64);
                if let Some(section) = table.get_mut(name.as_str()).and_then(|v| v.as_table_mut()) {
                    section.insert("priority".into(), prio.clone());
                }
                // Custom provider path: [providers.custom.<name>]
                if let Some(custom_table) = table
                    .get_mut("providers")
                    .and_then(|p| p.as_table_mut())
                    .and_then(|p| p.get_mut("custom"))
                    .and_then(|c| c.as_table_mut())
                {
                    if let Some(section) = custom_table
                        .get_mut(name.as_str())
                        .and_then(|v| v.as_table_mut())
                    {
                        section.insert("priority".into(), prio);
                    }
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

    /// Fire a background GET against `source` with the optional header;
    /// result lands in self.api_test for the poll loop to collect.
    fn test_api(&mut self, source: &str, header: &str) {
        use std::sync::mpsc;
        if source.is_empty() {
            self.status = "API test: endpoint is empty".into();
            return;
        }
        let header = header.to_string();
        let source = source.to_string();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| -> Result<String, String> {
                let mut req = ureq::get(&source).timeout(std::time::Duration::from_secs(8));
                if !header.trim().is_empty() {
                    // Expand ${ENV} and split "Key: value".
                    let expanded = expand_env_str(&header);
                    let mut parts = expanded.splitn(2, ':');
                    let key = parts.next().unwrap_or("").trim();
                    let val = parts.next().unwrap_or("").trim();
                    if !key.is_empty() {
                        req = req.set(key, val);
                    }
                }
                let body = req
                    .call()
                    .map_err(|e| format!("{e}"))?
                    .into_string()
                    .map_err(|e| format!("read failed: {e}"))?;
                // Pretty-print JSON when possible for the preview.
                match serde_json::from_str::<serde_json::Value>(&body) {
                    Ok(v) => serde_json::to_string_pretty(&v).map_err(|e| e.to_string()),
                    Err(_) => Ok(body.chars().take(2000).collect()),
                }
            })();
            let _ = tx.send(result);
        });
        self.api_test = Some(rx);
        self.status = "Testing API…".into();
    }

    /// Collect finished API test results (non-blocking).
    fn take_api_result(&mut self) -> Option<Result<String, String>> {
        let rx = self.api_test.as_mut()?;
        match rx.try_recv() {
            Ok(r) => {
                self.api_test = None;
                Some(r)
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.status = "Testing API…".into();
                None
            }
            Err(_) => {
                self.api_test = None;
                Some(Err("connection dropped".into()))
            }
        }
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
                                // Custom providers nest under providers.custom.<name>.
                                let is_custom = self
                                    .doc
                                    .get("providers")
                                    .and_then(|p| p.get("custom"))
                                    .and_then(|c| c.get(&name))
                                    .is_some();
                                let enabled_key = if is_custom {
                                    format!("providers.custom.{name}.enabled")
                                } else {
                                    format!("{name}.enabled")
                                };
                                let mut enabled = self.get_bool(&enabled_key);
                                if ui.checkbox(&mut enabled, "").changed() {
                                    self.set_bool(&enabled_key, enabled);
                                }

                                let label = ui.selectable_label(is_selected, name.as_str());

                                // Transparent interaction layer over the whole
                                // row (checkbox excluded) handling select+drag.
                                // Grow the hitbox only RIGHTWARD so it never
                                // overlaps the checkbox to its left.
                                let mut hb = label.rect;
                                hb.max.x += 60.0;
                                hb.min.y -= 4.0;
                                hb.max.y += 4.0;
                                let row_id = egui::Id::new(("provider_row", name));
                                let hitbox = ui.interact(hb, row_id, egui::Sense::click_and_drag());
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
                    // Send result back so the GUI's delivery poll can apply
                    // it. Without this send the receiver never fires and
                    // the status stays stuck on "Searching..." forever.
                    // Convert anyhow::Error to String to match the channel
                    // type the receiver expects.
                    let payload = result.map_err(|e| format!("{e}"));
                    let _ = tx.send(payload);
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

    // Enabled state lives in the sidebar checkbox; no duplicate here.
    ui.horizontal(|ui| {
        let mut hdr = app
            .get_value(&format!("{base}.show_header"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if ui.checkbox(&mut hdr, "Show provider name header").changed() {
            app.set_bool(&format!("{base}.show_header"), hdr);
        }
    });
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
    ui.horizontal(|ui| {
        let mut s = app.get_str(&format!("{base}.source"));
        let response =
            ui.add(egui::TextEdit::singleline(&mut s).desired_width(ui.available_width() - 70.0));
        if response.changed() || response.lost_focus() {
            app.set_str(&format!("{base}.source"), &s);
        }
        if ui.button("Test").clicked() {
            app.test_api(
                &s.trim().to_string(),
                &app.get_str(&format!("{base}.header")),
            );
        }
    });
    // Live API test result (raw JSON preview) polled from background thread.
    if let Some(result) = app.take_api_result() {
        match result {
            Ok(body) => {
                app.api_preview = Some(body.clone());
                // Auto-suggest field rows from the response structure.
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                    app.api_suggested = Some(suggest_fields(&v));
                }
                app.status = format!("API OK ({} bytes)", body.len());
            }
            Err(e) => {
                app.status = format!("API test failed: {e}");
            }
        }
    }

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
        if ui.toggle_value(&mut app.show_secret, "👁").changed() {
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
        let response = ui.add(egui::TextEdit::singleline(&mut s).desired_width(f32::INFINITY));
        if response.changed() || response.lost_focus() {
            app.set_str(path, &s);
        }
    });
}

#[derive(Clone, PartialEq)]
enum Align {
    Left,
    Center,
    Right,
}
impl Align {
    fn as_str(&self) -> &'static str {
        match self {
            Align::Left => "L",
            Align::Center => "C",
            Align::Right => "R",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "L" | "l" => Some(Align::Left),
            "C" | "c" => Some(Align::Center),
            "R" | "r" => Some(Align::Right),
            _ => None,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Align::Left => "Left",
            Align::Center => "Center",
            Align::Right => "Right",
        }
    }
}

#[derive(Clone, PartialEq)]
enum SizeCls {
    Small,
    Medium,
    Large,
    XLarge,
}
impl SizeCls {
    fn as_str(&self) -> &'static str {
        match self {
            SizeCls::Small => "S",
            SizeCls::Medium => "M",
            SizeCls::Large => "L",
            SizeCls::XLarge => "X",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "S" | "s" => Some(SizeCls::Small),
            "M" | "m" => Some(SizeCls::Medium),
            "L" | "l" => Some(SizeCls::Large),
            "X" | "x" => Some(SizeCls::XLarge),
            _ => None,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            SizeCls::Small => "S",
            SizeCls::Medium => "M",
            SizeCls::Large => "L",
            SizeCls::XLarge => "X",
        }
    }
}

#[derive(Clone)]
struct FieldRow {
    path: String,
    label: String,
    label_visible: bool,
    value_visible: bool,
    align: Align,
    size: SizeCls,
    /// Explicit y-row slot. None = auto-pack top-down in array order.
    row: Option<usize>,
}

impl FieldRow {
    /// Parse a legacy/new fields entry. Visibility is stored as explicit
    /// state, never inferred from the strings themselves.
    /// Parse a fields entry. Format: `[path][: <label>][!]` where:
    ///   * `!` after path = value hidden
    ///   * `-` in label slot = label hidden
    ///   * anything else in label slot = label visible with that text
    ///   * legacy `path` (no colon, no `!`) = both visible, label auto-derived
    ///   * legacy `path!` (no colon) = value hidden, label auto-derived
    fn parse(s: &str) -> Self {
        let s = s.trim();
        // Layout metadata (after " | "): space-separated k=v pairs.
        // Currently: a={L|C|R}, s={S|M|L}, r=<row index>
        let mut align = Align::Left;
        let mut size = SizeCls::Medium;
        let mut row: Option<usize> = None;
        let (head, layout) = match s.split_once('|') {
            Some((h, l)) => (h, l),
            None => (s, ""),
        };
        for kv in layout.split_whitespace() {
            if let Some((k, v)) = kv.split_once('=') {
                match k {
                    "a" => align = Align::parse(v).unwrap_or(Align::Left),
                    "s" => size = SizeCls::parse(v).unwrap_or(SizeCls::Medium),
                    "r" => row = v.parse::<usize>().ok(),
                    _ => {}
                }
            }
        }
        // Strip trailing `!` for value visibility.
        let (head, value_visible) = if let Some(stripped) = head.strip_suffix('!') {
            (stripped, false)
        } else {
            (head, true)
        };
        let (path, label_text, label_visible) = match head.split_once(':') {
            Some((p, l)) => {
                let l = l.trim();
                if l == "-" {
                    (p.trim().to_string(), String::new(), false)
                } else if l.is_empty() {
                    (p.trim().to_string(), String::new(), false)
                } else {
                    (p.trim().to_string(), l.to_string(), true)
                }
            }
            None => {
                let tail = head
                    .rsplit('.')
                    .next()
                    .unwrap_or(head)
                    .trim_end_matches("[0]")
                    .to_string();
                (head.to_string(), tail, true)
            }
        };
        FieldRow {
            path,
            label: label_text,
            label_visible,
            value_visible,
            align,
            size,
            row,
        }
    }

    fn auto_label(&self) -> String {
        self.path
            .rsplit('.')
            .next()
            .unwrap_or(&self.path)
            .trim_end_matches("[0]")
            .to_string()
    }

    fn effective_label(&self) -> String {
        if self.label_visible {
            if self.label.is_empty() {
                self.auto_label()
            } else {
                self.label.clone()
            }
        } else {
            String::new()
        }
    }

    /// Serialize back to TOML. Always emits `path: <label>` form so
    /// `parse` can recover visibility on round-trip regardless of how
    /// the user toggled checkboxes:
    ///   * label_visible=false → emit `-`
    ///   * label_visible=true, empty label → emit auto-derived label
    ///   * label_visible=true, text label → emit that text
    /// A trailing `!` after the label marks value_visible=false.
    fn to_toml_string(&mut self) -> String {
        let label_slot = if !self.label_visible {
            String::from("-")
        } else if self.label.is_empty() {
            // Auto-derive so we have something concrete to write.
            let auto = self.auto_label();
            self.label = auto.clone();
            auto
        } else {
            self.label.clone()
        };
        let p = self.path.trim();
        let suffix = if self.value_visible { "" } else { "!" };
        let layout = self.layout_suffix();
        if layout.is_empty() {
            format!("{p}: {label_slot}{suffix}")
        } else {
            format!("{p}: {label_slot}{suffix} | {layout}")
        }
    }

    /// Build the trailing "a=… s=… r=…" string for non-default layout
    /// values. Empty string means "all defaults; don't write metadata".
    fn layout_suffix(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.align != Align::Left {
            parts.push(format!("a={}", self.align.as_str()));
        }
        if self.size != SizeCls::Medium {
            parts.push(format!("s={}", self.size.as_str()));
        }
        if let Some(r) = self.row {
            parts.push(format!("r={r}"));
        }
        parts.join(" ")
    }
}

fn edit_fields_table(ui: &mut egui::Ui, app: &mut App, base: &str) {
    let fields_path = format!("{base}.fields");
    let mut rows: Vec<FieldRow> = Vec::new();
    if let Some(toml::Value::Array(arr)) = app.get_value_owned(&fields_path) {
        for v in arr {
            if let Some(s) = v.as_str() {
                rows.push(FieldRow::parse(s));
            }
        }
    }

    let mut changed = false;
    let mut remove_idx: Option<usize> = None;
    let drag_from = app.field_drag_from;
    let mut drag_over = app.field_drag_over;
    let mut row_rects: Vec<(usize, egui::Rect)> = Vec::new();

    for (i, row) in rows.iter_mut().enumerate() {
        // Capture row rect BEFORE drawing widgets so drag-hover detection
        // sees the actual rendered area.
        let row_resp = ui.horizontal(|ui| {
            // Column 1: label visibility checkbox.
            if ui.checkbox(&mut row.label_visible, "").changed() {
                changed = true;
            }

            // Column 2: label text input (always editable).
            let key_resp = ui.add(
                egui::TextEdit::singleline(&mut row.label)
                    .hint_text("label")
                    .desired_width(90.0),
            );
            if key_resp.changed() {
                changed = true;
            }

            // Column 3: JSON path / value.
            let p_resp = ui.add(
                egui::TextEdit::singleline(&mut row.path)
                    .hint_text("json.path[0].key")
                    .desired_width(150.0),
            );
            if p_resp.changed() || p_resp.lost_focus() {
                changed = true;
            }

            // Column 4: value visibility checkbox.
            if ui.checkbox(&mut row.value_visible, "").changed() {
                changed = true;
            }

            // Column 4.5: layout controls (align, size, row slot).
            // Rendered as a compact horizontal block so each row stays
            // single-line.
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let prev = row.align.clone();
                egui::ComboBox::from_id_source(("align", i))
                    .selected_text(row.align.label())
                    .show_ui(ui, |ui| {
                        for a in [Align::Left, Align::Center, Align::Right] {
                            if ui.selectable_label(row.align == a, a.label()).clicked() {
                                row.align = a.clone();
                                changed = true;
                            }
                        }
                    });
                if prev != row.align {
                    changed = true;
                }
                let prev = row.size.clone();
                egui::ComboBox::from_id_source(("size", i))
                    .selected_text(row.size.label())
                    .show_ui(ui, |ui| {
                        for s in [
                            SizeCls::Small,
                            SizeCls::Medium,
                            SizeCls::Large,
                            SizeCls::XLarge,
                        ] {
                            if ui.selectable_label(row.size == s, s.label()).clicked() {
                                row.size = s.clone();
                                changed = true;
                            }
                        }
                    });
                if prev != row.size {
                    changed = true;
                }
                // Row slot: 0-5 (panel is 40px tall, 1 row ≈ 8-14px depending on size)
                let mut row_str = row.row.map(|n| n.to_string()).unwrap_or_default();
                let r_resp = ui.add(
                    egui::TextEdit::singleline(&mut row_str)
                        .hint_text("row")
                        .desired_width(28.0),
                );
                if r_resp.changed() || r_resp.lost_focus() {
                    let trimmed = row_str.trim();
                    row.row = if trimmed.is_empty() {
                        None
                    } else {
                        trimmed.parse::<usize>().ok().map(|n| n.min(5))
                    };
                    changed = true;
                }
            });

            // Column 5: drag handle — sole drag initiator.
            let handle = ui.add(egui::Button::new("⠿").small().sense(egui::Sense::drag()));
            if handle.drag_started() {
                app.field_drag_from = Some(i);
                app.field_drag_active = true;
                app.field_drag_pending_commit = false;
            }
            if handle.drag_stopped() {
                app.field_drag_pending_commit = true;
            }
            if handle.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
            if handle.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                app.field_drag_active = true;
            } else if app.field_drag_from == Some(i) {
                // We're not being dragged anymore this frame — that means
                // the drag ended (drag_stopped may have fired on a previous
                // frame already, or this is the release frame).
                if !handle.drag_stopped() {
                    app.field_drag_active = false;
                }
            }
            handle.on_hover_text("Drag to reorder field");

            // Column 6: remove.
            if ui.button("✕").clicked() {
                remove_idx = Some(i);
            }
        });
        row_rects.push((i, row_resp.response.rect));
    }

    // Drag-over detection: pointer position vs each row's full rect.
    if drag_from.is_some() {
        if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
            app.field_drag_over = row_rects
                .iter()
                .find(|(_, r)| r.contains(pos))
                .map(|(i, _)| *i);
        }
    }

    ui.add_space(2.0);
    ui.horizontal(|ui| {
        if ui.button("+ Add field").clicked() {
            rows.push(FieldRow {
                path: String::new(),
                label: String::new(),
                label_visible: true,
                value_visible: true,
                align: Align::Left,
                size: SizeCls::Medium,
                row: None,
            });
            changed = true;
        }
        if app.api_preview.is_some() && ui.button("Auto-fill from response").clicked() {
            if let Some(sugg) = &app.api_suggested {
                rows.clear();
                rows.extend(sugg.iter().map(|(p, l)| FieldRow {
                    path: p.clone(),
                    label: l.clone(),
                    label_visible: true,
                    value_visible: true,
                    align: Align::Left,
                    size: SizeCls::Medium,
                    row: None,
                }));
                changed = true;
            }
        }
    });

    // Commit drag reorder only when the user releases the handle.
    // During the drag we update drag_over every frame for hover feedback,
    // but the actual reorder happens once on release. Committing every
    // frame would re-trigger the reorder continuously while the pointer
    // is still down, which scrambles indices.
    let dragging_idx = (0..rows.len()).find(|&i| {
        // We can't query a previously-rendered handle here (it's gone),
        // so use the persisted drag_from as the indicator of "is a drag
        // currently in progress from this row". On release, drag_from is
        // cleared by the closure we set up below via drag_stopped.
        app.field_drag_from == Some(i)
    });
    // Commit only if we are NOT currently dragging — i.e., the frame after
    // the drag ended. We detect release by checking if any of our rendered
    // handles reports drag_stopped. Use a side-channel written below.
    if !app.field_drag_active && app.field_drag_pending_commit {
        if let (Some(from), Some(to)) = (app.field_drag_from, app.field_drag_over) {
            if from != to && from < rows.len() && to < rows.len() {
                let item = rows.remove(from);
                rows.insert(to, item);
                changed = true;
            }
        }
        app.field_drag_from = None;
        app.field_drag_over = None;
        app.field_drag_pending_commit = false;
    }
    let _ = dragging_idx;

    if let Some(idx) = remove_idx {
        rows.remove(idx);
        changed = true;
    }

    if changed {
        // Normalize each row (derive labels for visible-but-empty), then write.
        let serialized: Vec<toml::Value> = rows
            .iter_mut()
            .map(|r| toml::Value::String(r.to_toml_string()))
            .collect();
        app.set_value(&fields_path, toml::Value::Array(serialized));
    }

    // Last API response — collapsible panel below the fields so users can
    // see the raw JSON returned by their endpoint (and verify which paths
    // to fill into the fields above).
    if let Some(body) = &app.api_preview {
        ui.add_space(4.0);
        let pretty = serde_json::from_str::<serde_json::Value>(body)
            .map(|v| serde_json::to_string_pretty(&v).unwrap_or_else(|_| body.clone()))
            .unwrap_or_else(|_| body.clone());
        egui::CollapsingHeader::new("Last API response")
            .default_open(false)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(180.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut pretty.clone())
                                .code_editor()
                                .desired_width(f32::INFINITY),
                        );
                    });
            });
    }
}

/// Walk a JSON value and produce (path, label) suggestions for leaf scalars.
fn suggest_fields(v: &serde_json::Value) -> Vec<(String, String)> {
    fn walk(v: &serde_json::Value, prefix: &str, depth: usize, out: &mut Vec<(String, String)>) {
        if out.len() >= 12 {
            return;
        }
        match v {
            serde_json::Value::Object(map) => {
                if prefix.is_empty() && map.is_empty() {
                    return;
                }
                for (k, child) in map {
                    let p = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    walk(child, &p, depth + 1, out);
                }
            }
            serde_json::Value::Array(arr) => {
                if let Some(first) = arr.first() {
                    walk(first, &format!("{prefix}[0]"), depth + 1, out);
                }
            }
            // Leaf scalar.
            _ => {
                if !prefix.is_empty() {
                    let label = prefix
                        .rsplit('.')
                        .next()
                        .unwrap_or(prefix)
                        .trim_end_matches("[0]")
                        .to_string();
                    out.push((prefix.to_string(), label));
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(v, "", 0, &mut out);
    out
}

/// Expand `${VAR}` references from the process environment (GUI side).
fn expand_env_str(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                out.push_str(std::env::var(&after[..end]).unwrap_or_default().as_str());
                rest = &after[end + 1..];
            }
            None => {
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
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
            .with_inner_size([900.0, 620.0])
            .with_min_inner_size([820.0, 540.0])
            .with_title("ss-oled settings"),
        ..Default::default()
    };
    eframe::run_native("ss-oled settings", native, Box::new(|_cc| Box::new(app)))
        .map_err(|e| anyhow::anyhow!("eframe error: {e}"))
}
