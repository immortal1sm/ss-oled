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
    dirs_or_home().join(".config/ss-oled/settings.toml")
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
    /// Working text for the custom-provider rename field. Kept out of the TOML
    /// doc so typing never rewrites the file on its own.
    rename_buffer: Option<String>,
    /// Custom provider awaiting confirmation before deletion, if any.
    pending_remove: Option<String>,
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
    /// Whether the raw "Last API response" box is expanded. Opened by
    /// clicking a field row, since that is when you want to see the JSON to
    /// work out the right path.
    show_api_response: bool,
    /// Receiver for the in-flight API test.
    api_test: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    /// Hotkey field currently waiting for the next key press.
    recording_hotkey: Option<String>,
    /// Numpad flag per hotkey config key (next/previous/lock_toggle).
    /// egui reports `Key::Num1` for BOTH the top-row and numpad 1 (its own
    /// docs say "Either from the main row or from the numpad"), so the
    /// recorder cannot infer this. It must be an explicit user choice, or
    /// recorded bindings silently become top-row keys that never reach the
    /// kernel on keyboards whose top row is not delivered.
    hotkey_numpad_next: bool,
    hotkey_numpad_previous: bool,
    hotkey_numpad_lock: bool,
    /// Same egui top-row-vs-numpad caveat applies to the custom-API controls.
    hotkey_numpad_item_next: bool,
    hotkey_numpad_item_previous: bool,
    hotkey_numpad_scroll_up: bool,
    hotkey_numpad_scroll_down: bool,
    hotkey_numpad_detail_toggle: bool,
    hotkey_numpad_notification_lock: bool,
    hotkey_numpad_eightball: bool,
    /// Field-editor drag state (persists across frames while dragging).
    field_drag_from: Option<usize>,
    /// Set when the debug checkbox is ticked. The daemon fixes its log level
    /// when the logger is initialised, so the change only lands on restart;
    /// this drives the confirmation before doing it.
    debug_restart_prompt: bool,
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

        // Read existing hotkey strings up front so the numpad checkboxes
        // reflect what is already in the config.
        let hotkey_str = |k: &str| {
            doc.get("hotkeys")
                .and_then(|h| h.get(k))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let prev_hk = hotkey_str("previous");
        let next_hk = hotkey_str("next");
        let lock_hk = hotkey_str("lock_toggle");
        let item_prev_hk = hotkey_str("item_previous");
        let item_next_hk = hotkey_str("item_next");
        let scroll_up_hk = hotkey_str("scroll_up");
        let scroll_down_hk = hotkey_str("scroll_down");
        let detail_hk = hotkey_str("detail_toggle");
        let notif_lock_hk = hotkey_str("notification_lock");
        let eightball_hk = hotkey_str("eightball");

        let mut app = Self {
            config_path: path,
            doc,
            providers: Vec::new(),
            selected: None,
            rename_buffer: None,
            pending_remove: None,
            drag_from: None,
            drag_over: None,
            city_query: String::new(),
            province_filter,
            show_secret: false,
            api_preview: None,
            api_suggested: None,
            show_api_response: false,
            api_test: None,
            recording_hotkey: None,
            hotkey_numpad_previous: hotkey_is_numpad(&prev_hk),
            hotkey_numpad_next: hotkey_is_numpad(&next_hk),
            hotkey_numpad_lock: hotkey_is_numpad(&lock_hk),
            hotkey_numpad_item_next: hotkey_is_numpad(&item_next_hk),
            hotkey_numpad_item_previous: hotkey_is_numpad(&item_prev_hk),
            hotkey_numpad_scroll_up: hotkey_is_numpad(&scroll_up_hk),
            hotkey_numpad_scroll_down: hotkey_is_numpad(&scroll_down_hk),
            hotkey_numpad_detail_toggle: hotkey_is_numpad(&detail_hk),
            hotkey_numpad_notification_lock: hotkey_is_numpad(&notif_lock_hk),
            hotkey_numpad_eightball: hotkey_is_numpad(&eightball_hk),
            field_drag_from: None,
            debug_restart_prompt: false,
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

    fn capture_hotkey_recording(&mut self, ctx: &egui::Context) {
        let Some(path) = self.recording_hotkey.clone() else {
            return;
        };
        let captured = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Key {
                    key,
                    physical_key: _,
                    pressed: true,
                    repeat: false,
                    modifiers,
                } => {
                    // Escape cancels recording instead of binding itself.
                    if *key == egui::Key::Escape {
                        return Some(Recorded::Cancel);
                    }
                    format_recorded_hotkey(*key, *modifiers).map(Recorded::Combo)
                }
                _ => None,
            })
        });
        match captured {
            Some(Recorded::Combo(combo)) => {
                // egui reports the same Key for a numpad key and its top-row
                // twin, so the recorded name is applied through the explicit
                // numpad flag. Without this, recording always produces a
                // top-row binding.
                let use_numpad = match path.as_str() {
                    "hotkeys.next" => self.hotkey_numpad_next,
                    "hotkeys.previous" => self.hotkey_numpad_previous,
                    "hotkeys.lock_toggle" => self.hotkey_numpad_lock,
                    "hotkeys.item_next" => self.hotkey_numpad_item_next,
                    "hotkeys.item_previous" => self.hotkey_numpad_item_previous,
                    "hotkeys.scroll_up" => self.hotkey_numpad_scroll_up,
                    "hotkeys.scroll_down" => self.hotkey_numpad_scroll_down,
                    "hotkeys.detail_toggle" => self.hotkey_numpad_detail_toggle,
                    "hotkeys.notification_lock" => self.hotkey_numpad_notification_lock,
                    "hotkeys.eightball" => self.hotkey_numpad_eightball,
                    "hotkeys.eightball" => self.hotkey_numpad_eightball,
                    _ => false,
                };
                let combo = if use_numpad {
                    apply_numpad(&combo, true)
                } else {
                    combo
                };
                self.set_str(&path, &combo);
                self.recording_hotkey = None;
                self.status = format!("Recorded {combo}");
            }
            Some(Recorded::Cancel) => {
                self.recording_hotkey = None;
                self.status = "Recording cancelled".to_string();
            }
            None => {}
        }
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

        // Sibling providers nested under [providers.*] that are not custom
        // (e.g. [providers.lyrics]) have no `enabled` at the top level, so the
        // scan above misses them. Pick them up explicitly.
        if let Some(providers) = self
            .doc
            .get("providers")
            .and_then(|p| p.as_table())
        {
            for (name, v) in providers {
                // `custom` is handled above as a table-of-tables.
                if name == "custom" {
                    continue;
                }
                if v.get("enabled").and_then(|e| e.as_bool()).is_some() {
                    let prio = v.get("priority").and_then(|p| p.as_integer()).unwrap_or(99);
                    list.push((name.clone(), prio));
                }
            }
        }

        list.sort_by_key(|(_, prio)| *prio);
        // A provider can match more than one scan: `[providers.lyrics]` is
        // found by the sibling scan, and a stray top-level `[lyrics]` (written
        // by an older build's sync_priorities) is found by the top-level scan.
        // That produced two identical sidebar rows that both resolved to the
        // same `selected` name, so neither could be clicked distinctly. Keep the
        // first (lowest priority) entry per name.
        let mut seen: Vec<String> = Vec::new();
        list.retain(|(name, _)| {
            if seen.iter().any(|n| n == name) {
                // apex-gui has no logging backend; stderr matches its
                // existing "apex-gui: loaded ..." startup line.
                eprintln!("apex-gui: duplicate provider '{name}' ignored (check config for a stray section)");
                false
            } else {
                seen.push(name.clone());
                true
            }
        });
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
                .args(["--user", "restart", "ss-oled"])
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
    /// survives save + daemon restart. Providers live in up to three places:
    ///   - built-ins at the top level: `[sysinfo]`
    ///   - custom providers: `[providers.custom.<name>]`
    ///   - other nested providers: `[providers.<name>]` (e.g. `[providers.lyrics]`)
    /// All three must be walked or a reorder silently snaps back on restart —
    /// the original bug for the custom case, reintroduced when `[providers.*]`
    /// siblings were added.
    fn sync_priorities(&mut self) {
        let names: Vec<String> = self.providers.clone();
        if let Some(table) = self.doc.as_table_mut() {
            for (idx, name) in names.iter().enumerate() {
                let prio = toml::Value::Integer((idx + 1) as i64);
                if let Some(section) = table.get_mut(name.as_str()).and_then(|v| v.as_table_mut()) {
                    section.insert("priority".into(), prio.clone());
                }
                let Some(providers) = table.get_mut("providers").and_then(|p| p.as_table_mut())
                else {
                    continue;
                };
                // `[providers.<name>]` — nested provider like `lyrics`.
                if let Some(section) = providers.get_mut(name.as_str()).and_then(|v| v.as_table_mut())
                {
                    section.insert("priority".into(), prio.clone());
                }
                // `[providers.custom.<name>]` — custom provider.
                if let Some(custom) = providers.get_mut("custom").and_then(|c| c.as_table_mut()) {
                    if let Some(section) =
                        custom.get_mut(name.as_str()).and_then(|v| v.as_table_mut())
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

    /// Buffered text for the custom-provider rename field. Kept out of the
    /// TOML doc so typing never rewrites the file.
    fn doc_has_custom(&self, name: &str) -> bool {
        self.doc
            .get("providers")
            .and_then(|p| p.get("custom"))
            .and_then(|c| c.get(name))
            .is_some()
    }

    /// Rename `[providers.custom.<from>]` to `[providers.custom.<to>]`.
    ///
    /// TOML has no rename, so the table is moved and its `interval.<from>`
    /// dwell entry follows it — otherwise the renamed provider silently falls
    /// back to the global refresh interval.
    fn rename_custom(&mut self, from: &str, to: &str) -> anyhow::Result<()> {
        if self.doc_has_custom(to) {
            return Err(anyhow::anyhow!("'{to}' already exists"));
        }
        if let Some(custom) = self
            .doc
            .get_mut("providers")
            .and_then(|p| p.get_mut("custom"))
            .and_then(|c| c.as_table_mut())
        {
            if let Some(tbl) = custom.remove(from) {
                custom.insert(to.to_string(), tbl);
            }
        }
        if let Some(iv) = self
            .doc
            .get_mut("interval")
            .and_then(|i| i.as_table_mut())
        {
            if let Some(v) = iv.remove(from) {
                iv.insert(to.to_string(), v);
            }
        }
        self.save()
    }

    /// Delete `[providers.custom.<name>]` and its dwell entry.
    fn remove_custom(&mut self, name: &str) -> anyhow::Result<()> {
        if let Some(custom) = self
            .doc
            .get_mut("providers")
            .and_then(|p| p.get_mut("custom"))
            .and_then(|c| c.as_table_mut())
        {
            custom.remove(name);
        }
        if let Some(iv) = self
            .doc
            .get_mut("interval")
            .and_then(|i| i.as_table_mut())
        {
            iv.remove(name);
        }
        self.save()
    }

    /// Append a new `[providers.custom.<name>]` section with sane defaults,
    /// so a new JSON-API screen only needs its URL and field paths.
    fn add_custom(&mut self, name: &str) -> anyhow::Result<()> {
        if name.trim().is_empty() || self.doc_has_custom(name) {
            return Err(anyhow::anyhow!("empty or duplicate name"));
        }
        let mut tbl = toml::Table::new();
        tbl.insert("enabled".into(), toml::Value::Boolean(true));
        tbl.insert("fields".into(), toml::Value::Array(vec![toml::Value::String(
            "[0].value".into(),
        )]));
        tbl.insert("interval".into(), toml::Value::Integer(30));
        tbl.insert("poll".into(), toml::Value::Integer(300));
        tbl.insert("priority".into(), toml::Value::Integer(5));
        tbl.insert("show_header".into(), toml::Value::Boolean(true));
        tbl.insert(
            "source".into(),
            toml::Value::String("https://example.com/api/data.json".into()),
        );
        // Create `[providers]` and `[providers.custom]` if they are not there
        // yet. The old code only inserted when `custom` already existed, so
        // "Add custom" silently did NOTHING on a config that had no custom
        // providers -- no row, no error, the section just vanished. It worked
        // before only because an existing section happened to be present.
        let mut root = match self.doc.as_table_mut() {
            Some(t) => t,
            None => return Err(anyhow::anyhow!("config document is not a table")),
        };
        let providers = root
            .entry("providers".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let providers = match providers.as_table_mut() {
            Some(t) => t,
            None => return Err(anyhow::anyhow!("`providers` is not a table")),
        };
        // A stray non-table `custom` key would make this fail; replace it.
        let custom = providers
            .entry("custom".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let custom = match custom.as_table_mut() {
            Some(t) => t,
            None => {
                let fresh = toml::Value::Table(toml::Table::new());
                providers.insert("custom".to_string(), fresh);
                providers
                    .get_mut("custom")
                    .and_then(|c| c.as_table_mut())
                    .ok_or_else(|| anyhow::anyhow!("could not create `providers.custom`"))?
            }
        };
        custom.insert(name.to_string(), toml::Value::Table(tbl));
        self.save()
    }

    /// Add a custom JSON-API provider. Returns the new section name.
    fn add_custom_unique(&mut self, base: &str) -> anyhow::Result<String> {
        let mut candidate = base.to_string();
        let mut n = 1;
        while self.doc_has_custom(&candidate) {
            candidate = format!("{base}{n}");
            n += 1;
        }
        self.add_custom(&candidate)?;
        Ok(candidate)
    }
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
        self.capture_hotkey_recording(ctx);
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

                // Add a custom JSON-API provider. The section is created with
                // defaults and selected immediately, so the user only has to
                // fill in the URL and field paths on the right.
                if ui
                    .button("+ Add custom (API)")
                    .on_hover_text("Create a new custom JSON-API provider section")
                    .clicked()
                {
                    match self.add_custom_unique("custom") {
                        Ok(new_name) => {
                            self.selected = Some(new_name.clone());
                            self.rename_buffer = None;
                            self.refresh_provider_list();
                            self.status = format!("Added custom provider '{new_name}'");
                        }
                        Err(e) => self.status = format!("Add failed: {e}"),
                    }
                }


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
                                // A provider's config path depends on where its
                                // section lives: top-level (`[sysinfo]`),
                                // custom (`[providers.custom.<name>]`), or a
                                // nested provider (`[providers.<name>]`, e.g.
                                // lyrics). Guessing "custom or top-level"
                                // wrote `lyrics.enabled` at the top level,
                                // creating a stray section that then showed up
                                // as a second, unclickable sidebar row.
                                let providers = self.doc.get("providers");
                                let is_custom = providers
                                    .and_then(|p| p.get("custom"))
                                    .and_then(|c| c.get(&name))
                                    .is_some();
                                let is_nested = providers.and_then(|p| p.get(&name)).is_some();
                                let enabled_key = if is_custom {
                                    format!("providers.custom.{name}.enabled")
                                } else if is_nested {
                                    format!("providers.{name}.enabled")
                                } else {
                                    format!("{name}.enabled")
                                };
                                let mut enabled = self.get_bool(&enabled_key);
                                if ui.checkbox(&mut enabled, "").changed() {
                                    self.set_bool(&enabled_key, enabled);
                                }

                                // The Button carries click AND drag sense
                                // itself. A separate transparent `ui.interact`
                                // layer on top of it stole the pointer, so the
                                // row never got the hover highlight the
                                // "Add custom" button has.
                                let resp = ui.add(
                                    egui::Button::new(name.as_str())
                                        .selected(is_selected)
                                        .sense(egui::Sense::click_and_drag())
                                        .min_size(egui::vec2(PROVIDER_ROW_BUTTON_W, 0.0)),
                                );
                                if resp.clicked() {
                                    self.selected = Some(name.clone());
                                    self.pending_remove = None;
                                }
                                if resp.drag_started() {
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

                // Everything below is NOT a provider. Notifications are an
                // overlay: they interrupt whatever the rotation is showing,
                // are never rotated to, and have no priority/dwell/enabled.
                // Grouping them under their own heading keeps that distinction
                // visible instead of implying they sit in the rotation.
                ui.separator();
                ui.label(egui::RichText::new("OVERLAYS").small().weak());
                if ui
                    .add(
                        egui::Button::new("Notifications")
                            .selected(self.selected.as_deref() == Some("__notifications")),
                    )
                    .on_hover_text("Interrupts the current screen when one arrives")
                    .clicked()
                {
                    self.selected = Some("__notifications".to_string());
                }
                if ui
                    .add(
                        egui::Button::new("8 Ball")
                            .selected(self.selected.as_deref() == Some("__eightball")),
                    )
                    .on_hover_text("Ask the 8 ball on demand — not part of the rotation")
                    .clicked()
                {
                    self.selected = Some("__eightball".to_string());
                }

                ui.separator();
                ui.label(egui::RichText::new("SETTINGS").small().weak());
                if ui
                    .add(
                        egui::Button::new("Hotkeys")
                            .selected(self.selected.as_deref() == Some("__hotkeys")),
                    )
                    .clicked()
                {
                    self.selected = Some("__hotkeys".to_string());
                }
            });

        // ---------- CENTER: selected provider settings ----------
        egui::CentralPanel::default().show(ctx, |ui| {
            match self.selected.clone() {
                Some(name) => {
                    let title = match name.as_str() {
                        "__hotkeys" => "Hotkeys",
                        "__notifications" => "Notifications",
                        "__eightball" => "8 Ball",
                        _ => name.as_str(),
                    };
                    ui.heading(title);
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
            if self.debug_restart_prompt {
                let mut open = true;
                egui::Window::new("Restart required")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .open(&mut open)
                    .show(ctx, |ui| {
                        ui.label(
                            "Debug logging applies after the daemon restarts.\n\nApply saves the setting and restarts the daemon now.",
                        );
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button("Restart daemon").clicked() {
                                self.debug_restart_prompt = false;
                                self.apply();
                            }
                            if ui.button("Later").clicked() {
                                self.debug_restart_prompt = false;
                                self.status = "Saved. Restart the daemon to apply".into();
                            }
                        });
                    });
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

                    // Pushed to the right edge of the action bar. The level is
                    // fixed when the daemon's logger is initialised, so this
                    // takes effect on Apply (which restarts the daemon) rather
                    // than immediately.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let mut dbg =
                            self.get_str("log.level").eq_ignore_ascii_case("debug");
                        if ui
                            .checkbox(&mut dbg, "Debug")
                            .on_hover_text(
                                "Verbose daemon logs. Adds a per-frame trace while a notification is on screen. Applies on Apply.",
                            )
                            .changed()
                        {
                            self.set_value(
                                "log.level",
                                toml::Value::String(if dbg { "debug".into() } else { "info".into() }),
                            );
                            self.debug_restart_prompt = true;
                        }
                    });
                });
            });
        });
    }
}

/// Shared width for every button in the left sidebar.
///
/// The sidebar is `default_width(240.0)`; 216 leaves the usual egui frame
/// margin so the buttons span the panel evenly instead of each one sizing to
/// its own label.
/// Shared width for the provider rows only, so they line up with each other.
/// Sized to sit comfortably beside the enabled-checkbox in the 240px sidebar
/// without the row reading as a full-width bar.
const PROVIDER_ROW_BUTTON_W: f32 = 165.0;

fn provider_section(ui: &mut egui::Ui, app: &mut App, name: &str) {
    if name == "__notifications" {
        notifications_editor(ui, app);
    } else if name == "__eightball" {
        eightball_editor(ui, app);
    } else if name == "__hotkeys" {
        hotkeys_editor(ui, app);
        return;
    }

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
            ui.label("Show for (s):")
                .on_hover_text("How long this screen stays before the rotation moves on");
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
        "lyrics" => {
            combo_field(
                ui,
                app,
                "providers.lyrics.source",
                "Lyrics source",
                &["auto", "local", "lrclib"],
            );
            combo_field(
                ui,
                app,
                "providers.lyrics.font",
                "Font size (auto = largest that fits)",
                &["auto", "S", "M", "L", "XL"],
            );
            combo_field(
                ui,
                app,
                "providers.lyrics.align",
                "Alignment",
                &["L", "C", "R"],
            );
            toggle(ui, app, "providers.lyrics.bold", "Bold text");
            toggle(
                ui,
                app,
                "providers.lyrics.show_title",
                "Show track title above lyrics",
            );
            ui.label(
                "auto sources local .lrc first, then lrclib.net. Cached under \
                 ~/.cache/ss-oled/lyrics/.",
            );
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

/// Settings for the on-demand 8 ball.
///
/// Lives in an `[eightball]` table. Deliberately NOT a provider: it has no
/// priority, no dwell and never enters the rotation — it only produces frames
/// when the hotkey is pressed, as an overlay.
fn eightball_editor(ui: &mut egui::Ui, app: &mut App) {
    ui.label(
        "Shown on demand when you press its hotkey. It is an overlay, not a \
         provider: it never enters the rotation and has no dwell or priority.",
    );
    ui.label(
        "The ball animates while the answer is fetched, then shows the reading \
         beside it. Requests are made only when the key is pressed.",
    );

    ui.separator();

    ui.horizontal(|ui| {
        ui.label("Reading duration (s):")
            .on_hover_text("How long the answer stays on screen after it arrives");
        let mut d = app.get_int("eightball.duration").max(1);
        if ui
            .add(egui::DragValue::new(&mut d).clamp_range(1..=300))
            .changed()
        {
            app.set_int("eightball.duration", d);
        }
    });

    ui.horizontal(|ui| {
        toggle(ui, app, "eightball.show_timer", "Countdown");
        toggle(ui, app, "eightball.timer_border", "Edge frame");
    });

    ui.separator();
    ui.label("Layout");
    ui.label(
        "The ball occupies the lower-left, so the reading starts to its right. \
         The app line is off by default: the ball already identifies the source.",
    );
    notif_line_editor_for(ui, app, "eightball", "app", "App:");
    notif_line_editor_for(ui, app, "eightball", "title", "Title:");
    notif_line_editor_for(ui, app, "eightball", "content", "Reading:");

    ui.separator();
    ui.label("Source");
    let mut url = app.get_str("eightball.url");
    if url.is_empty() {
        url = "https://eightballapi.com/api?locale=en".into();
    }
    ui.horizontal(|ui| {
        ui.label("URL:");
        ui.text_edit_singleline(&mut url);
        if ui.button("Reset").clicked() {
            url = "https://eightballapi.com/api?locale=en".into();
            app.set_str("eightball.url", &url);
        }
    });
    ui.label(
        "The API returns 403 without a User-Agent header; the daemon always \
         sends one, so no key or token is needed.",
    );
    ui.label("Restart the daemon for changes to take effect.");
}

/// Settings for desktop notifications.
///
/// Lives in a `[notifications]` table, read by the scheduler and the
/// notification renderer rather than by a provider.
fn notifications_editor(ui: &mut egui::Ui, app: &mut App) {
    toggle(
        ui,
        app,
        "notifications.override",
        "Show immediately (override rotation)",
    );

    ui.horizontal(|ui| {
        ui.label("Notification duration (s):")
            .on_hover_text("How long every notification is shown, whatever the app requests");
        let mut d = app
            .get_int("notifications.duration")
            .max(1);
        if ui
            .add(egui::DragValue::new(&mut d).clamp_range(1..=300))
            .changed()
        {
            app.set_int("notifications.duration", d);
        }
    });

    ui.separator();
    ui.label("Layout");
    notif_line_editor(ui, app, "app", "App:");
    notif_line_editor(ui, app, "title", "Title:");
    notif_line_editor(ui, app, "content", "Body:");
    ui.horizontal(|ui| {
        toggle(ui, app, "notifications.show_timer", "Countdown ring");
        toggle(
            ui,
            app,
            "notifications.timer_border",
            "Edge frame (else corner ring)",
        );
    });
    ui.label("row 0 = auto (packs below the previous line).");
    ui.label("dy nudges a line up or down. The edge frame leaves the whole panel free; the corner ring reserves space.");

}

/// Per-line editor for one notification text part.
fn notif_line_editor(ui: &mut egui::Ui, app: &mut App, part: &str, label: &str) {
    notif_line_editor_for(ui, app, "notifications", part, label)
}

/// Row editor for one text part of any overlay, under `<table>.lines.<part>`.
///
/// Shared by notifications and the 8 ball so a layout tweak applies to both.
/// `table` is the config root: "notifications" or "eightball".
fn notif_line_editor_for(
    ui: &mut egui::Ui,
    app: &mut App,
    table: &str,
    part: &str,
    label: &str,
) {
    let base = format!("{table}.lines.{part}");

    ui.horizontal(|ui| {
        ui.label(label);
        // The app line is opt-in, so its fallback is off; title/body are on.
        let fallback = part == "title" || part == "content";
        let mut shown = app
            .get_value(&format!("{base}.shown"))
            .and_then(|v| v.as_bool())
            .unwrap_or(fallback);
        if ui.checkbox(&mut shown, "").changed() {
            app.set_bool(&format!("{base}.shown"), shown);
        }

        // Content defaults to auto (it varies most); title and app have a
        // fixed default so an unset key keeps the original look.
        let default_size = match part {
            "content" => "auto",
            "title" => "l",
            _ => "m",
        };
        let mut size = app.get_str(&format!("{base}.size"));
        if size.is_empty() {
            size = default_size.into();
        }
        egui::ComboBox::from_label(format!("{label} size"))
            .selected_text(size.to_uppercase())
            .show_ui(ui, |ui| {
                for (val, name) in [
                    ("auto", "AUTO"),
                    ("s", "S"),
                    ("m", "M"),
                    ("l", "L"),
                    ("xl", "XL"),
                ] {
                    ui.selectable_value(&mut size, val.to_string(), name);
                }
            });
        if size != default_size {
            app.set_value(&format!("{base}.size"), toml::Value::String(size));
        }

        let mut align = app.get_str(&format!("{base}.align"));
        if align.is_empty() {
            align = "left".into();
        }
        egui::ComboBox::from_label(format!("{label} align"))
            .selected_text(align.clone())
            .show_ui(ui, |ui| {
                for opt in ["left", "center", "right"] {
                    ui.selectable_value(&mut align, opt.to_string(), opt);
                }
            });
        if align != "left" {
            app.set_value(&format!("{base}.align"), toml::Value::String(align));
        }

        ui.label("row:");
        let mut row = app.get_int(&format!("{base}.row"));
        if ui
            .add(egui::DragValue::new(&mut row).clamp_range(0..=38))
            .changed()
        {
            app.set_int(&format!("{base}.row"), row);
        }

        ui.label("dy:");
        let mut dy = app.get_int(&format!("{base}.dy"));
        if ui
            .add(egui::DragValue::new(&mut dy).clamp_range(-10..=10))
            .on_hover_text("Nudge this line up or down, in pixels")
            .changed()
        {
            app.set_int(&format!("{base}.dy"), dy);
        }

        // Only the body wraps; the title scrolls horizontally instead.
        if part == "content" {
            let mut wrap = app.get_bool(&format!("{base}.wrap"));
            if ui
                .checkbox(&mut wrap, "wrap")
                .on_hover_text("Wrap onto more lines using the space below")
                .changed()
            {
                app.set_bool(&format!("{base}.wrap"), wrap);
            }
        }

        let mut bold = app.get_bool(&format!("{base}.bold"));
        if ui
            .checkbox(&mut bold, "bold")
            .on_hover_text("Bold")
            .changed()
        {
            app.set_bool(&format!("{base}.bold"), bold);
        }
    });
}

fn hotkeys_editor(ui: &mut egui::Ui, app: &mut App) {
    ui.label("Click Record, press the desired combo, then Save/Apply. Clear disables that action.");
    ui.label(
        "Restart/apply is required because global shortcuts are registered at daemon startup.",
    );
    ui.add_space(8.0);

    // Copy the numpad flags out before the calls: `hotkey_text_field` needs
    // `&mut app`, so we cannot also hand it `&mut app.hotkey_numpad_*`.
    let mut numpad_previous = app.hotkey_numpad_previous;
    let mut numpad_next = app.hotkey_numpad_next;
    let mut numpad_lock = app.hotkey_numpad_lock;
    let mut numpad_item_next = app.hotkey_numpad_item_next;
    let mut numpad_item_prev = app.hotkey_numpad_item_previous;
    let mut numpad_scroll_up = app.hotkey_numpad_scroll_up;
    let mut numpad_scroll_down = app.hotkey_numpad_scroll_down;
    let mut numpad_detail = app.hotkey_numpad_detail_toggle;
    let mut numpad_notif_lock = app.hotkey_numpad_notification_lock;
    let mut numpad_eightball = app.hotkey_numpad_eightball;

    hotkey_text_field(
        ui,
        app,
        "hotkeys.previous",
        "Previous provider",
        "Ctrl+Shift+Numpad *",
        &mut numpad_previous,
    );
    hotkey_text_field(
        ui,
        app,
        "hotkeys.next",
        "Next provider",
        "Ctrl+Shift+Numpad /",
        &mut numpad_next,
    );
    hotkey_text_field(
        ui,
        app,
        "hotkeys.lock_toggle",
        "Lock / unlock provider",
        "Ctrl+Shift+Numpad -",
        &mut numpad_lock,
    );
    app.hotkey_numpad_previous = numpad_previous;
    app.hotkey_numpad_next = numpad_next;
    app.hotkey_numpad_lock = numpad_lock;

    // Custom-API list controls. Only meaningful while a custom provider with
    // `items` is on screen; elsewhere the hotkeys are a no-op.
    ui.add_space(8.0);
    ui.separator();
    ui.label("Custom API list");
    ui.label("Move between items, scroll a secondary screen, or open it.");
    hotkey_text_field(
        ui,
        app,
        "hotkeys.item_previous",
        "Previous item",
        "Ctrl+Alt+Left",
        &mut numpad_item_prev,
    );
    hotkey_text_field(
        ui,
        app,
        "hotkeys.item_next",
        "Next item",
        "Ctrl+Alt+Right",
        &mut numpad_item_next,
    );
    hotkey_text_field(
        ui,
        app,
        "hotkeys.scroll_up",
        "Scroll up",
        "Ctrl+Alt+Up",
        &mut numpad_scroll_up,
    );
    hotkey_text_field(
        ui,
        app,
        "hotkeys.scroll_down",
        "Scroll down",
        "Ctrl+Alt+Down",
        &mut numpad_scroll_down,
    );
    hotkey_text_field(
        ui,
        app,
        "hotkeys.detail_toggle",
        "Open / close secondary screen",
        "Ctrl+Alt+Numpad0",
        &mut numpad_detail,
    );
    hotkey_text_field(
        ui,
        app,
        "hotkeys.notification_lock",
        "Lock / unlock notification",
        "Ctrl+Alt+Numpad.",
        &mut numpad_notif_lock,
    );
    hotkey_text_field(
        ui,
        app,
        "hotkeys.eightball",
        "Ask the 8 ball",
        "Ctrl+Alt+Numpad8",
        &mut numpad_eightball,
    );
    app.hotkey_numpad_item_next = numpad_item_next;
    app.hotkey_numpad_item_previous = numpad_item_prev;
    app.hotkey_numpad_scroll_up = numpad_scroll_up;
    app.hotkey_numpad_scroll_down = numpad_scroll_down;
    app.hotkey_numpad_detail_toggle = numpad_detail;
    app.hotkey_numpad_notification_lock = numpad_notif_lock;
    app.hotkey_numpad_eightball = numpad_eightball;

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.button("Reset to defaults").clicked() {
            app.set_str("hotkeys.next", "Ctrl+Shift+Numpad /");
            app.set_str("hotkeys.previous", "Ctrl+Shift+Numpad *");
            app.set_str("hotkeys.lock_toggle", "Ctrl+Shift+Numpad -");
            app.set_str("hotkeys.item_previous", "Ctrl+Alt+Left");
            app.set_str("hotkeys.item_next", "Ctrl+Alt+Right");
            app.set_str("hotkeys.scroll_up", "Ctrl+Alt+Up");
            app.set_str("hotkeys.scroll_down", "Ctrl+Alt+Down");
            app.set_str("hotkeys.detail_toggle", "Ctrl+Alt+Numpad0");
            app.set_str("hotkeys.notification_lock", "Ctrl+Alt+Numpad.");
            app.set_str("hotkeys.eightball", "Ctrl+Alt+Numpad8");
            app.recording_hotkey = None;
            app.status = "Hotkeys reset to defaults".to_string();
        }
        if ui.button("Clear all").clicked() {
            app.set_str("hotkeys.next", "");
            app.set_str("hotkeys.previous", "");
            app.set_str("hotkeys.lock_toggle", "");
            app.recording_hotkey = None;
            app.status = "All hotkeys disabled".to_string();
        }
    });

    ui.add_space(8.0);
    ui.label(
        "Tick Numpad for keys on the numeric keypad. egui cannot tell a numpad key \
         from its top-row twin, so recording needs this hint — without it a recorded \
         binding is always a top-row key.",
    );
    ui.label(
        "Supported keys: Numpad / * - + and 0-9, letters A-Z, F1-F12, arrows, \
         Enter, Space, Tab, Escape.",
    );
}

fn hotkey_text_field(
    ui: &mut egui::Ui,
    app: &mut App,
    key: &str,
    label: &str,
    default: &str,
    numpad: &mut bool,
) {
    ui.horizontal(|ui| {
        ui.label(format!("{label}:"));
        let mut s = app.get_str(key);
        let response = ui.add(
            egui::TextEdit::singleline(&mut s)
                .hint_text(default)
                .desired_width(240.0),
        );
        if response.changed() || response.lost_focus() {
            app.set_str(key, &s);
            // Keep the checkbox honest when the text is edited by hand.
            *numpad = hotkey_is_numpad(&s);
        }
        let is_recording = app.recording_hotkey.as_deref() == Some(key);
        let record_label = if is_recording {
            "Recording…"
        } else {
            "Record"
        };
        if ui.button(record_label).clicked() {
            app.recording_hotkey = Some(key.to_string());
            app.status = format!("Press a key combo for {label}");
        }
        if ui.button("Clear").clicked() {
            app.set_str(key, "");
            if is_recording {
                app.recording_hotkey = None;
            }
            app.status = format!("{label} hotkey disabled");
        }
        // egui cannot tell numpad from top row, so this is explicit.
        if ui.checkbox(numpad, egui::RichText::new("Numpad").small()).changed() {
            let cur = app.get_str(key);
            let updated = apply_numpad(&cur, *numpad);
            app.set_str(key, &updated);
            app.status = format!(
                "{label} numpad {}",
                if *numpad { "on" } else { "off" }
            );
        }
    });
}

/// Does this binding string name a numpad key?
fn hotkey_is_numpad(spec: &str) -> bool {
    spec.split('+').any(|p| {
        let p = p.trim().to_ascii_lowercase();
        p.starts_with("numpad")
    })
}

/// Insert or remove the `Numpad` prefix on the final key segment of a binding.
/// Idempotent in both directions: applying the same state twice is a no-op, and
/// a literal trailing `+` key (e.g. "Ctrl++") survives a round trip.
fn apply_numpad(spec: &str, on: bool) -> String {
    let segs: Vec<&str> = spec.split('+').collect();
    if segs.is_empty() {
        return spec.to_string();
    }
    let mut out: Vec<String> = segs[..segs.len() - 1]
        .iter()
        .map(|s| s.trim().to_string())
        .collect();

    // The final segment is empty when the binding ends in a literal "+" key
    // (split on '+' turns "Ctrl++" into ["Ctrl", "", ""]).
    let last = segs[segs.len() - 1].trim();
    let (last, literal_plus) = if last.is_empty() {
        ("+", true)
    } else {
        (last, false)
    };

    // Strip a leading "numpad" PREFIX (not a char set - trim_start_matches
    // would eat the letters of "Numpad" one by one and also mangle keys that
    // merely start with those characters).
    let lower = last.to_ascii_lowercase();
    let stripped = if lower == "numpad" {
        String::new()
    } else if let Some(rest) = lower.strip_prefix("numpad") {
        rest.trim().to_string()
    } else {
        last.to_string()
    };

    let new_last = if on {
        if stripped.is_empty() {
            if literal_plus {
                // Nothing to prefix onto a bare "+" key; keep the key alone.
                "+".to_string()
            } else {
                "Numpad".to_string()
            }
        } else {
            format!("Numpad{stripped}")
        }
    } else {
        if literal_plus {
            "+".to_string()
        } else {
            stripped
        }
    };

    // A trailing literal '+' must stay a separate segment so the daemon's
    // parser (which preserves a trailing '+' before splitting) sees it.
    out.push(new_last);
    if literal_plus {
        out.push(String::new());
    }
    out.join("+")
}

/// Outcome of a single recording attempt.
enum Recorded {
    Combo(String),
    Cancel,
}

fn format_recorded_hotkey(
    key: egui::Key,
    modifiers: egui::Modifiers,
) -> Option<String> {
    if matches!(key, egui::Key::Copy | egui::Key::Cut | egui::Key::Paste) {
        return None;
    }
    let mut parts: Vec<&'static str> = Vec::new();
    if modifiers.ctrl {
        parts.push("Ctrl");
    }
    if modifiers.alt {
        parts.push("Alt");
    }
    if modifiers.shift {
        parts.push("Shift");
    }
    if modifiers.mac_cmd {
        parts.push("Super");
    }
    parts.push(recorded_key_name(key)?);
    Some(parts.join("+"))
}

fn recorded_key_name(key: egui::Key) -> Option<&'static str> {
    Some(match key {
        egui::Key::ArrowDown => "ArrowDown",
        egui::Key::ArrowLeft => "ArrowLeft",
        egui::Key::ArrowRight => "ArrowRight",
        egui::Key::ArrowUp => "ArrowUp",
        egui::Key::Escape => "Escape",
        egui::Key::Tab => "Tab",
        egui::Key::Backspace => "Backspace",
        egui::Key::Enter => "Enter",
        egui::Key::Space => "Space",
        egui::Key::Slash => "/",
        egui::Key::Minus => "-",
        // The numpad decimal is the one key the daemon needs spelled out
        // rather than as bare ".": its parser matches the bare period as a
        // TOP-ROW key, so a recorded "Ctrl+Alt+." would bind the wrong one.
        egui::Key::Period => ".",
        egui::Key::Plus => "+",
        egui::Key::Num0 => "0",
        egui::Key::Num1 => "1",
        egui::Key::Num2 => "2",
        egui::Key::Num3 => "3",
        egui::Key::Num4 => "4",
        egui::Key::Num5 => "5",
        egui::Key::Num6 => "6",
        egui::Key::Num7 => "7",
        egui::Key::Num8 => "8",
        egui::Key::Num9 => "9",
        egui::Key::A => "A",
        egui::Key::B => "B",
        egui::Key::C => "C",
        egui::Key::D => "D",
        egui::Key::E => "E",
        egui::Key::F => "F",
        egui::Key::G => "G",
        egui::Key::H => "H",
        egui::Key::I => "I",
        egui::Key::J => "J",
        egui::Key::K => "K",
        egui::Key::L => "L",
        egui::Key::M => "M",
        egui::Key::N => "N",
        egui::Key::O => "O",
        egui::Key::P => "P",
        egui::Key::Q => "Q",
        egui::Key::R => "R",
        egui::Key::S => "S",
        egui::Key::T => "T",
        egui::Key::U => "U",
        egui::Key::V => "V",
        egui::Key::W => "W",
        egui::Key::X => "X",
        egui::Key::Y => "Y",
        egui::Key::Z => "Z",
        egui::Key::F1 => "F1",
        egui::Key::F2 => "F2",
        egui::Key::F3 => "F3",
        egui::Key::F4 => "F4",
        egui::Key::F5 => "F5",
        egui::Key::F6 => "F6",
        egui::Key::F7 => "F7",
        egui::Key::F8 => "F8",
        egui::Key::F9 => "F9",
        egui::Key::F10 => "F10",
        egui::Key::F11 => "F11",
        egui::Key::F12 => "F12",
        _ => return None,
    })
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

/// Dropdown restricted to a fixed set of string values.
///
/// Unlike `text_field`, this cannot produce a value the provider would reject,
/// which matters for enum-like settings (lyrics source/font/alignment).
fn combo_field(ui: &mut egui::Ui, app: &mut App, key: &str, label: &str, options: &[&str]) {
    ui.horizontal(|ui| {
        ui.label(format!("{label}:"));
        let current = app.get_str(key);
        // Fall back to the first option when unset or holding a stale value.
        let selected = options.iter().position(|o| *o == current).unwrap_or(0);
        let mut idx = selected;

        egui::ComboBox::from_id_source(key)
            .selected_text(options[selected])
            .show_ui(ui, |ui| {
                for (i, opt) in options.iter().enumerate() {
                    ui.selectable_value(&mut idx, i, *opt);
                }
            });

        if idx != selected {
            app.set_str(key, options[idx]);
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

    // Rename / remove. Both rewrite the `[providers.custom.<name>]` table
    // itself, so they need the whole TOML document rather than a single key —
    // set_bool/set_value can only touch one leaf at a time.
    ui.horizontal(|ui| {
        ui.label("Name:");
        app.rename_buffer.get_or_insert_with(|| name.to_string());
        ui.text_edit_singleline(app.rename_buffer.as_mut().unwrap())
            .on_hover_text("Rename this custom provider");

        // Read the candidate out of the buffer before calling &self methods:
        // the buffer borrow would otherwise conflict with them.
        let candidate = app.rename_buffer.clone().unwrap_or_default();
        let target = candidate.trim().to_string();
        let clashes = app.doc_has_custom(&target);
        let can_rename = !target.is_empty() && target != name && !clashes;

        if ui
            .add_enabled(can_rename, egui::Button::new("Rename"))
            .on_hover_text(if clashes {
                "A custom provider with that name already exists"
            } else {
                "Rename the section in settings.toml"
            })
            .clicked()
        {
            match app.rename_custom(name, &target) {
                Ok(()) => {
                    app.refresh_provider_list();
                    app.status = format!("Renamed '{name}' to '{target}'");
                    app.rename_buffer = None;
                    app.selected = Some(target);
                }
                Err(e) => app.status = format!("Rename failed: {e}"),
            }
        }

        // Deleting drops a whole config section, so it is two-step: the first
        // click arms a confirm, the second deletes. Any other selection or a
        // rename cancels it.
        if app.pending_remove.as_deref() == Some(name) {
            ui.colored_label(egui::Color32::from_rgb(255, 120, 120), "Delete?");
            if ui.button("Confirm delete").clicked() {
                match app.remove_custom(name) {
                    Ok(()) => {
                        // The sidebar list is derived from the TOML doc;
                        // without this the row survives until restart.
                        app.refresh_provider_list();
                        app.selected = None;
                        app.rename_buffer = None;
                        app.pending_remove = None;
                        app.status = format!("Removed custom provider '{name}'");
                    }
                    Err(e) => app.status = format!("Remove failed: {e}"),
                }
            }
            if ui.button("Cancel").clicked() {
                app.pending_remove = None;
            }
        } else if ui
            .add(egui::Button::new("Remove"))
            .on_hover_text("Delete this custom provider from settings.toml")
            .clicked()
        {
            app.pending_remove = Some(name.to_string());
        }
    });

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
        ui.label("Show for (s):")
            .on_hover_text("How long this screen stays before the rotation moves on");
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
                // A fresh response is exactly when the user wants to read it.
                app.show_api_response = true;
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
    edit_fields_table(ui, app, &base, "fields");

    // ---- List mode ----------------------------------------------------
    // When the API returns an array, each element becomes its own screen and
    // the Left/Right hotkeys step between them. `items` is the JSON path to
    // that array; leave it blank for an API that returns a single object.
    ui.add_space(6.0);
    ui.separator();

    // Explicit toggles rather than inferring behaviour from which keys are
    // present: a spare field list should not silently turn on a second screen.
    // The checkbox only reveals/hides the array controls -- it must NOT write
    // a placeholder path. Writing the literal string "items" produced a config
    // pointing at a key that does not exist, so the fetch found no array and
    // the panel showed NO DATA with no error anywhere.
    let mut list_on = !app.get_str(&format!("{base}.items")).is_empty();
    if ui
        .checkbox(&mut list_on, "This API returns multiple items (array)")
        .on_hover_text("Reveals the array path, max items and the secondary screen below")
        .changed()
        && !list_on
    {
        // Turning it off clears the path so the provider falls back to
        // single-object rendering rather than looking for a stale array.
        app.set_str(&format!("{base}.items"), "");
    }
    if !list_on {
        ui.label("   Single-object API — one screen, fields above only.");
        ui.add_space(6.0);
        ui.separator();
    }

    if list_on {
    ui.horizontal(|ui| {
        ui.label("Array path:");
        let path = format!("{base}.items");
        let mut v = app.get_str(&path);
        if ui
            .add(
                egui::TextEdit::singleline(&mut v)
                    .desired_width(160.0)
                    .hint_text("e.g. hits — blank for a single object"),
            )
            .on_hover_text(
                "JSON path to the array to iterate. Each element renders as its own screen.",
            )
            .changed()
        {
            app.set_str(&path, v.trim());
        }
    });
    ui.horizontal(|ui| {
        ui.label("Max items:");
        let path = format!("{base}.max_items");
        let mut n = app.get_int(&path).max(1) as i32;
        if ui
            .add(egui::DragValue::new(&mut n).clamp_range(1..=200))
            .on_hover_text("How many array elements to keep from the response")
            .changed()
        {
            app.set_int(&path, n as i64);
        }
    });

    }

    // ---- Secondary screen ---------------------------------------------
    // Only meaningful for an array API: the secondary screen renders the
    // selected element's body instead of advancing to the next element.
    if list_on {
        ui.add_space(6.0);
        ui.separator();
        let mut article_on = app.get_bool(&format!("{base}.article"));
        if ui
            .checkbox(&mut article_on, "Add a secondary screen (article view)")
            .on_hover_text(
                "Ctrl+Alt+Numpad0 opens the selected item's body instead of the next item. Ctrl+Alt+Up/Down scrolls it.",
            )
            .changed()
        {
            app.set_bool(&format!("{base}.article"), article_on);
        }
        if article_on {
            edit_fields_table(ui, app, &base, "article_fields");
            ui.label("Shown after Ctrl+Alt+Numpad0, scrolled with Ctrl+Alt+Up/Down.");
        } else {
            ui.label("   Ctrl+Alt+Left / Right move between items.");
        }
    }

    // One raw-response box for the whole provider, rendered once at the
    // bottom. It used to live inside the field-table editor, which meant a
    // copy appeared under every table as soon as the editor was shared.
    ui.add_space(6.0);
    ui.separator();
    if app.api_preview.is_some() {
        let pretty = serde_json::from_str::<serde_json::Value>(&app.api_preview.clone().unwrap())
            .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
            .unwrap_or_else(|_| app.api_preview.clone().unwrap());
        // egui 0.27's CollapsingHeader has no `show_open`; toggling the header
        // updates the flag, and the flag drives `default_open` next frame.
        let resp = egui::CollapsingHeader::new("Last API response")
            .id_source("last_api_response")
            .default_open(app.show_api_response)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(180.0)
                    .show(ui, |ui| {
                        ui.monospace(pretty);
                    });
            });
        if resp.header_response.clicked() {
            app.show_api_response = !app.show_api_response;
        }
    } else {
        ui.label("No API response yet — use Test API to fetch one.");
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SizeCls {
    Small,
    Medium,
    Large,
    XLarge,
    /// Largest class the field's text fits in. The daemon resolves it per
    /// render against the space available below the field.
    Auto,
}
impl SizeCls {
    fn as_str(&self) -> &'static str {
        match self {
            SizeCls::Small => "S",
            SizeCls::Medium => "M",
            SizeCls::Large => "L",
            SizeCls::XLarge => "X",
            SizeCls::Auto => "A",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "S" | "s" => Some(SizeCls::Small),
            "M" | "m" => Some(SizeCls::Medium),
            "L" | "l" => Some(SizeCls::Large),
            "A" | "a" => Some(SizeCls::Auto),
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
            SizeCls::Auto => "A",
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
    /// Render with a faux-bold double-strike.
    bold: bool,
    /// Vertical nudge in pixels; negative moves up. Mirrors
    /// `notifications.lines.*.dy`.
    dy: i32,
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
        let mut bold = false;
        let mut dy: i32 = 0;
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
                    "b" => bold = matches!(v, "1" | "true" | "yes" | "on"),
                    // Signed: `d=-2` nudges up, `d=2` nudges down.
                    "d" => dy = v.parse::<i32>().unwrap_or(0),
                    _ => {}
                }
            }
        }
        // Strip trailing `!` for value visibility. The TRIM matters: splitting
        // on `|` leaves `"path: Label "` with a trailing space, so without it
        // `strip_suffix('!')` never matches and a hidden value silently
        // reappears on any field that also carries layout metadata.
        let head_trimmed = head.trim_end();
        let (head, value_visible) = if let Some(stripped) = head_trimmed.strip_suffix('!') {
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
            bold,
            dy,
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

    /// Build the trailing "a=… s=… r=… b=… d=…" string for non-default layout
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
        if self.bold {
            parts.push("b=1".to_string());
        }
        if self.dy != 0 {
            parts.push(format!("d={}", self.dy));
        }
        parts.join(" ")
    }
}

/// Editor for one field list.
///
/// `key` names the config array to edit, relative to `base` — `fields` for
/// the highlight list, `article_fields` for the secondary screen. Passing a
/// key keeps both lists identical in behaviour without duplicating the whole
/// table (drag rows, dy nudge, visibility flags, suggest-from-response).
fn edit_fields_table(ui: &mut egui::Ui, app: &mut App, base: &str, key: &str) {
    let fields_path = format!("{base}.{key}");
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
            // Clicking a path is the moment you want the raw JSON to work out
            // what to put there, so reveal it. Done on the REAL row: an
            // earlier version rendered a second copy of every path purely as
            // a click target, which showed up as a stray duplicate list
            // between the table and the preview once fields existed.
            let path_resp = ui
                .add(
                    egui::TextEdit::singleline(&mut row.path)
                        .hint_text("json.path[0].key")
                        .desired_width(150.0),
                )
                .on_hover_text("Click to show the last API response");
            if path_resp.clicked() {
                app.show_api_response = true;
            }
            if path_resp.changed() || path_resp.lost_focus() {
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
                egui::ComboBox::from_id_source(("align", key, i))
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
                egui::ComboBox::from_id_source(("size", key, i))
                    .selected_text(row.size.label())
                    .show_ui(ui, |ui| {
                        for s in [
                            SizeCls::Auto,
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
                // Bold toggle: faux-bold via double-strike at the daemon.
                if ui.selectable_label(row.bold, "B").clicked() {
                    row.bold = !row.bold;
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

                // Vertical nudge: negative moves up, positive down. A
                // DragValue keeps this consistent with the notification
                // panel's `dy` and stops the free-text box silently discarding
                // anything that isn't a plain integer.
                let mut dy = row.dy;
                if ui
                    .add(
                        egui::DragValue::new(&mut dy)
                            .clamp_range(-10..=10)
                            .speed(0.25),
                    )
                    .on_hover_text("Nudge this field up (negative) or down (positive), in pixels")
                    .changed()
                {
                    row.dy = dy;
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
                bold: false,
                dy: 0,
            });
            changed = true;
        }
        // Always rendered. Previously this only appeared once a response had
        // been fetched, so on a freshly opened GUI the button simply was not
        // there and it read as missing rather than as "nothing to fill from".
        let have_response = app.api_preview.is_some();
        let clicked = ui
            .add_enabled(
                have_response,
                egui::Button::new("Auto-fill from response"),
            )
            .on_hover_text(if have_response {
                "Replace this list with the fields found in the last API response"
            } else {
                "Run Test API first — there is no response to read fields from"
            })
            .clicked();
        if clicked {
            if let Some(sugg) = &app.api_suggested {
                // The article table's paths resolve INSIDE each array element,
                // so suggestions rooted at the response (e.g. "hits.0.title")
                // have to be re-rooted at the element ("title") or every row
                // would resolve to nothing.
                // In array mode BOTH field lists resolve inside one element, so
                // suggestions rooted at the response ("hits.0.title") must be
                // re-rooted for either table. Leaving the prefix on either one
                // yields rows that resolve to nothing.
                let items_path = app.get_str(&format!("{base}.items"));
                let strip = (!items_path.is_empty()).then(|| items_path);
                rows.clear();
                rows.extend(sugg.iter().map(|(p, l)| FieldRow {
                    path: match &strip {
                        Some(root) => strip_root(p, root),
                        None => p.clone(),
                    },
                    label: l.clone(),
                    label_visible: true,
                    value_visible: true,
                    align: Align::Left,
                    size: SizeCls::Medium,
                    row: None,
                    bold: false,
                    dy: 0,
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

    ui.add_space(8.0);
    custom_oled_preview(ui, app, base, &rows);


}

fn draw_oled_canvas(ui: &mut egui::Ui, draw_fn: impl FnOnce(&egui::Painter, egui::Rect, f32)) {
    ui.label("Live OLED preview");
    let scale = 3.0_f32;
    let size = egui::vec2(128.0 * scale, 40.0 * scale);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, egui::Color32::BLACK);
    painter.rect_stroke(
        rect,
        2.0,
        egui::Stroke::new(1.0_f32, egui::Color32::from_gray(80)),
    );
    draw_fn(&painter, rect, scale);
}

fn custom_oled_preview(ui: &mut egui::Ui, app: &App, base: &str, rows: &[FieldRow]) {
    let provider_name = base.rsplit('.').next().unwrap_or("custom");
    let show_header = app
        .get_value(&format!("{base}.show_header"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    draw_oled_canvas(ui, |painter, rect, scale| {
        let mut y = 0_i32;
        if show_header {
            draw_preview_text(
                painter,
                rect,
                provider_name.to_uppercase().as_str(),
                0,
                y,
                SizeCls::Large,
                false,
                scale,
            );
            y += 12;
        } else {
            y = 2;
        }

        let Some(values) = preview_values_from_last_response(app, rows) else {
            draw_preview_text(
                painter,
                rect,
                "NO DATA",
                38,
                if show_header { 14 } else { 12 },
                SizeCls::XLarge,
                false,
                scale,
            );
            return;
        };

        let mut plan: Vec<(usize, i32)> = Vec::new();
        let mut taken_rows = [false; 6];
        let mut next_auto_y = y;

        for (idx, row) in rows.iter().enumerate() {
            if idx >= values.len() || (!row.label_visible && !row.value_visible) {
                continue;
            }
            let char_w = preview_char_w(&row.size);
            let line_h = preview_line_h(&row.size);
            let row_y = match row.row {
                Some(slot) if slot < taken_rows.len() && !taken_rows[slot] => {
                    taken_rows[slot] = true;
                    let target = match row.size {
                        SizeCls::XLarge => slot as i32 * 18,
                        SizeCls::Large => slot as i32 * 14,
                        SizeCls::Small => slot as i32 * 6,
                        SizeCls::Medium | SizeCls::Auto => slot as i32 * 8,
                    };
                    target.max(y)
                }
                _ => {
                    let t = next_auto_y;
                    let (label, value) = &values[idx];
                    let label_text = if row.label_visible && !label.is_empty() {
                        format!("{label}:")
                    } else {
                        String::new()
                    };
                    let label_w = char_w * label_text.chars().count() as i32;
                    let reserved_left = if label_text.is_empty() {
                        0
                    } else {
                        label_w + 4
                    };
                    let avail_w = (128 - reserved_left).max(8);
                    let max_lines = if t < 40 {
                        (((40 - t) / line_h) as usize).min(3)
                    } else {
                        0
                    };
                    let lines = if value.is_empty() || max_lines == 0 {
                        1
                    } else {
                        preview_wrap_text(value, avail_w, char_w, max_lines).len()
                    };
                    next_auto_y += (lines as i32) * line_h;
                    t
                }
            };
            plan.push((idx, row_y));
        }

        for (idx, row_y) in plan {
            let row = &rows[idx];
            let (label, value) = &values[idx];
            let char_w = preview_char_w(&row.size);
            let line_h = preview_line_h(&row.size);
            let label_text = if row.label_visible && !label.is_empty() {
                format!("{label}:")
            } else {
                String::new()
            };
            let text = if row.value_visible {
                value.clone()
            } else {
                String::new()
            };
            let label_w = char_w * label_text.chars().count() as i32;
            let reserved_left = if label_text.is_empty() {
                0
            } else {
                label_w + 4
            };
            let avail_w = (128 - reserved_left).max(8);
            let max_lines = if row_y < 40 {
                (((40 - row_y) / line_h) as usize).min(3)
            } else {
                0
            };
            let wrapped = if text.is_empty() {
                vec![String::new()]
            } else if row.row.is_none() && max_lines > 0 {
                preview_wrap_text(&text, avail_w, char_w, max_lines)
            } else {
                let take = (avail_w / char_w).max(0) as usize;
                vec![text.chars().take(take).collect()]
            };
            let first_w = wrapped
                .first()
                .map(|line| char_w * line.chars().count() as i32)
                .unwrap_or(0);
            let total_w = reserved_left + first_w;
            let x_offset = match row.align {
                Align::Left => 0,
                Align::Center => (128 - total_w).max(0) / 2,
                Align::Right => 128 - total_w,
            };
            if !label_text.is_empty() {
                draw_preview_text(
                    painter,
                    rect,
                    &label_text,
                    x_offset,
                    row_y,
                    row.size.clone(),
                    row.bold,
                    scale,
                );
            }
            let value_x = if label_text.is_empty() {
                x_offset
            } else {
                x_offset + label_w + 4
            };
            for (line_idx, line) in wrapped.iter().enumerate() {
                if line.is_empty() {
                    continue;
                }
                let y_pos = row_y + (line_idx as i32) * line_h;
                if y_pos + line_h > 40 {
                    break;
                }
                draw_preview_text(
                    painter,
                    rect,
                    line,
                    value_x,
                    y_pos,
                    row.size.clone(),
                    row.bold,
                    scale,
                );
            }
        }
    });
}

fn preview_values_from_last_response(
    app: &App,
    rows: &[FieldRow],
) -> Option<Vec<(String, String)>> {
    let body = app.api_preview.as_ref()?;
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    let mut values = Vec::new();
    for row in rows
        .iter()
        .filter(|row| row.label_visible || row.value_visible)
    {
        let label = row.effective_label();
        let value = if row.path.trim().is_empty() {
            "—".to_string()
        } else {
            preview_get_path(&json, row.path.trim())
                .map(preview_value_to_string)
                .unwrap_or_else(|| "—".to_string())
        };
        values.push((label, value));
    }
    Some(values)
}

fn preview_get_path<'a>(json: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = json;
    for seg in path.split('.') {
        if seg.is_empty() {
            continue;
        }
        if let Some((key, idx)) = seg.split_once('[') {
            if !key.is_empty() {
                cur = cur.get(key.trim_end_matches('['))?;
            }
            let idx: usize = idx.trim_end_matches(']').parse().ok()?;
            cur = cur.get(idx)?;
        } else {
            cur = cur.get(seg)?;
        }
    }
    Some(cur)
}

fn preview_value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Mirror of the daemon's `FieldSize::resolve`, so the live OLED preview
/// picks the SAME class the panel will actually draw. If the two disagreed,
/// the preview would show a layout the hardware never renders.
fn resolve_auto(size: &SizeCls, text: &str, avail_w: i32, avail_h: i32) -> SizeCls {
    if *size != SizeCls::Auto {
        return *size;
    }
    let chars = text.chars().count() as i32;
    let avail_w = avail_w.max(8);
    let avail_h = avail_h.max(0);
    for c in [SizeCls::XLarge, SizeCls::Large, SizeCls::Medium, SizeCls::Small] {
        let lh = preview_line_h(&c);
        if lh > avail_h {
            continue;
        }
        let per_line = (avail_w / preview_char_w(&c)).max(1);
        let lines = ((chars + per_line - 1) / per_line).max(1);
        if lines * lh <= avail_h {
            return c;
        }
    }
    SizeCls::Small
}

fn preview_char_w(size: &SizeCls) -> i32 {
    match size {
        SizeCls::Small => 4,
        SizeCls::Large => 6,
        SizeCls::XLarge => 8,
        // Auto must be resolved before this point; Medium is the safe
        // fallback so an unresolved value still previews something sane.
        SizeCls::Medium | SizeCls::Auto => 5,
    }
}

fn preview_line_h(size: &SizeCls) -> i32 {
    match size {
        SizeCls::XLarge => preview_char_w(size) + 6,
        _ => preview_char_w(size) + 2,
    }
}

fn preview_font_size(size: &SizeCls, scale: f32) -> f32 {
    match size {
        SizeCls::Small => 5.0 * scale,
        SizeCls::Large => 8.0 * scale,
        SizeCls::XLarge => 10.0 * scale,
        SizeCls::Medium | SizeCls::Auto => 6.0 * scale,
    }
}

fn draw_preview_text(
    painter: &egui::Painter,
    panel: egui::Rect,
    text: &str,
    x: i32,
    y: i32,
    size: SizeCls,
    bold: bool,
    scale: f32,
) {
    let pos = panel.left_top() + egui::vec2(x as f32 * scale, y as f32 * scale);
    let font = egui::FontId::monospace(preview_font_size(&size, scale));
    let color = egui::Color32::WHITE;
    painter.text(pos, egui::Align2::LEFT_TOP, text, font.clone(), color);
    if bold {
        painter.text(
            pos + egui::vec2(scale, 0.0),
            egui::Align2::LEFT_TOP,
            text,
            font,
            color,
        );
    }
}

fn preview_wrap_text(text: &str, max_px: i32, char_w: i32, max_lines: usize) -> Vec<String> {
    if max_lines == 0 {
        return vec![];
    }
    if text.is_empty() {
        return vec![String::new()];
    }
    let max_chars = (max_px / char_w).max(1) as usize;
    let mut lines = Vec::new();
    let mut remaining = text.to_string();
    while lines.len() < max_lines {
        if remaining.chars().count() <= max_chars {
            lines.push(remaining);
            return lines;
        }
        let mut prefix_end_byte = remaining.len();
        for (i, (byte_idx, _ch)) in remaining.char_indices().enumerate() {
            if i == max_chars {
                prefix_end_byte = byte_idx;
                break;
            }
        }
        let prefix = &remaining[..prefix_end_byte];
        let split_chars = match prefix.rfind(' ') {
            Some(byte_idx) if byte_idx > 0 => prefix[..byte_idx].chars().count(),
            _ => max_chars,
        };
        let first: String = remaining.chars().take(split_chars).collect();
        remaining = remaining
            .chars()
            .skip(split_chars)
            .collect::<String>()
            .trim_start()
            .to_string();
        if first.is_empty() {
            break;
        }
        lines.push(first);
    }
    if !remaining.is_empty() {
        let take = max_chars.saturating_sub(1);
        let truncated: String = remaining.chars().take(take).collect();
        lines.push(format!("{truncated}…"));
    }
    lines
}

/// Walk a JSON value and produce (path, label) suggestions for leaf scalars.
/// Re-root a suggested path from the response onto one array element.
///
/// Suggestions are generated from the whole response, so for a list API they
/// look like `hits.0.title`. The article/secondary field set is resolved
/// INSIDE each element, so it needs `title`. Paths that do not start with the
/// array root are returned untouched rather than mangled.
fn strip_root(path: &str, root: &str) -> String {
    let root_with_index = format!("{root}.0");
    if let Some(rest) = path.strip_prefix(&root_with_index) {
        return rest.trim_start_matches('.').to_string();
    }
    if let Some(rest) = path.strip_prefix(root) {
        return rest.trim_start_matches('.').to_string();
    }
    path.to_string()
}

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
            .with_inner_size([1020.0, 660.0])
            .with_min_inner_size([1000.0, 560.0])
            .with_title("ss-oled settings"),
        ..Default::default()
    };
    eframe::run_native("ss-oled settings", native, Box::new(|_cc| Box::new(app)))
        .map_err(|e| anyhow::anyhow!("eframe error: {e}"))
}

#[cfg(test)]
mod add_custom_tests {
    use super::*;

    /// The exact table-creation logic `add_custom` uses, so this asserts the
    /// real behaviour rather than a re-implementation of it.
    fn insert_new_custom(doc: &mut toml::Value, name: &str, tbl: toml::Value) -> anyhow::Result<()> {
        let mut root = doc.as_table_mut().ok_or_else(|| anyhow::anyhow!("not a table"))?;
        let providers = root
            .entry("providers".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let providers = providers.as_table_mut().ok_or_else(|| anyhow::anyhow!("not a table"))?;
        let custom = providers
            .entry("custom".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let custom = custom.as_table_mut().ok_or_else(|| anyhow::anyhow!("not a table"))?;
        custom.insert(name.to_string(), tbl);
        Ok(())
    }

    fn sample() -> toml::Value {
        let mut t = toml::Table::new();
        t.insert("enabled".into(), toml::Value::Boolean(true));
        toml::Value::Table(t)
    }

    /// Regression: "Add custom" silently did nothing when
    /// `[providers.custom]` did not already exist -- the insert was inside an
    /// `if let` with no else branch.
    #[test]
    fn add_custom_works_with_no_existing_custom_table() {
        for start in [
            "clock = 1",                       // config with no providers at all
            "[providers.lyrics]
enabled = true", // providers exists, custom does not
            "[providers.custom]
",              // empty custom table
        ] {
            let mut doc: toml::Value = toml::from_str(start).expect("parse");
            insert_new_custom(&mut doc, "custom", sample())
                .unwrap_or_else(|e| panic!("failed on {start:?}: {e}"));
            assert!(
                doc.get("providers").and_then(|p| p.get("custom")).and_then(|c| c.get("custom")).is_some(),
                "section was not created starting from {start:?}"
            );
        }
    }

    /// A second add must not clobber the first.
    #[test]
    fn adding_twice_keeps_both() {
        let mut doc = toml::Value::Table(toml::Table::new());
        insert_new_custom(&mut doc, "custom", sample()).unwrap();
        insert_new_custom(&mut doc, "custom1", sample()).unwrap();
        let custom = doc.get("providers").and_then(|p| p.get("custom")).unwrap();
        assert!(custom.get("custom").is_some(), "first add lost");
        assert!(custom.get("custom1").is_some(), "second add lost");
    }

    /// A non-table `custom` key must not wedge the button forever.
    #[test]
    fn an_existing_custom_table_does_not_block_adding() {
        let mut doc: toml::Value =
            toml::from_str("[providers.custom]\nenabled = true\n").expect("parse");
        insert_new_custom(&mut doc, "custom", sample())
            .expect("an existing custom table must not block adding");
        assert!(doc.get("providers").and_then(|p| p.get("custom")).and_then(|c| c.get("custom")).is_some());
    }
}

#[cfg(test)]
mod field_spec_tests {
    use super::*;

    /// The fields entry is a serialized format that the GUI re-parses every
    /// frame, so parse -> serialize must be lossless. A round-trip that flips a
    /// bool or drops `dy` makes a control look non-functional the moment the
    /// user types in it.
    /// Auto-fill on the SECONDARY table must re-root paths onto one array
    /// element. Suggestions come from the whole response ("hits.0.title"),
    /// but secondary fields resolve inside each element ("title"). Without
    /// this every auto-filled secondary row resolves to nothing.
    #[test]
    fn strip_root_re_roots_paths_onto_an_element() {
        assert_eq!(strip_root("hits.0.title", "hits"), "title");
        assert_eq!(strip_root("hits.0._highlightResult.title.value", "hits"),
                   "_highlightResult.title.value");
        assert_eq!(strip_root("hits.0.a.b.c", "hits"), "a.b.c");
        // Already element-relative: leave alone.
        assert_eq!(strip_root("title", "hits"), "title");
        // A different array must not be mangled into a bogus path.
        assert_eq!(strip_root("other.0.title", "hits"), "other.0.title");
        // Only ONE level of index is stripped, not a repeated prefix.
        assert_eq!(strip_root("hits.0.hits.0.title", "hits"), "hits.0.title");
    }

    #[test]
    fn field_spec_roundtrip_preserves_dy() {
        for dy in [-10, -2, -1, 0, 1, 2, 10] {
            let base = "[0].temperature_2m";
            let mut row = FieldRow::parse(&format!("{base} | d={dy}"));
            assert_eq!(row.dy, dy, "parse lost dy={dy}");
            let mut row = row;
            let out = row.to_toml_string();
            let again = FieldRow::parse(&out);
            assert_eq!(again.dy, dy, "round-trip changed dy={dy} (spec={out})");
            assert_eq!(again.path, base, "round-trip changed path");
        }
    }

    #[test]
    fn field_spec_roundtrip_preserves_all_layout() {
        // NB: the size token is `X` for XL, not `XL` — that is the format.
        let spec = "[0].value: Label | a=C s=X r=2 b=1 d=-3";
        let mut row = FieldRow::parse(spec);
        assert_eq!(row.align, Align::Center);
        assert_eq!(row.size, SizeCls::XLarge);
        assert_eq!(row.row, Some(2));
        assert!(row.bold);
        assert_eq!(row.dy, -3);
        let mut row = row;
        let again = FieldRow::parse(&row.to_toml_string());
        assert_eq!(again.align, row.align);
        assert_eq!(again.size, row.size);
        assert_eq!(again.row, row.row);
        assert_eq!(again.bold, row.bold);
        assert_eq!(again.dy, row.dy);
        assert_eq!(again.label, row.label);
        assert_eq!(again.label_visible, row.label_visible);
        assert_eq!(again.value_visible, row.value_visible);
    }

    /// A default field must serialize with NO layout suffix, so legacy entries
    /// stay byte-identical and untouched configs are not rewritten.
    #[test]
    fn field_spec_default_has_no_layout_suffix() {
        let mut row = FieldRow::parse("[0].title: My Label");
        assert_eq!(row.dy, 0);
        assert_eq!(row.to_toml_string(), "[0].title: My Label");
    }

    /// `dy` must survive alongside the visibility encodings, which are the
    /// part of this format that historically corrupted on round-trip.
    #[test]
    fn field_spec_roundtrip_with_hidden_value() {
        let spec = "[0].v: Lbl! | d=2";
        let row = FieldRow::parse(spec);
        assert!(!row.value_visible);
        assert_eq!(row.dy, 2);
        let mut row = row;
        let mut row = row;
        let again = FieldRow::parse(&row.to_toml_string());
        assert!(!again.value_visible, "value_visible flipped");
        assert_eq!(again.dy, 2);
    }
}
