//! Shared weather data fetching + caching for the weather and forecast
//! providers. One Open-Meteo call serves both; the fetch runs in a blocking
//! task every 15 minutes and publishes into a shared slot.

use anyhow::Result;
use serde_json::Value;
use std::{
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

/// WMO weather interpretation codes, grouped into display conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    Clear,
    PartlyCloudy,
    Overcast,
    Fog,
    Rain,
    Snow,
    Thunderstorm,
}

impl Condition {
    pub fn from_wmo(code: i64) -> Self {
        match code {
            0 => Condition::Clear,
            1 | 2 => Condition::PartlyCloudy,
            3 => Condition::Overcast,
            45 | 48 => Condition::Fog,
            51..=67 | 80..=82 => Condition::Rain,
            71..=77 | 85 | 86 => Condition::Snow,
            95..=99 => Condition::Thunderstorm,
            _ => Condition::Overcast,
        }
    }

    /// Short text for the OLED (fits ~10 chars at FONT_4X6).
    pub fn label(&self) -> &'static str {
        match self {
            Condition::Clear => "Clear",
            Condition::PartlyCloudy => "P.Cldy",
            Condition::Overcast => "Cloudy",
            Condition::Fog => "Fog",
            Condition::Rain => "Rain",
            Condition::Snow => "Snow",
            Condition::Thunderstorm => "Storm",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DayForecast {
    pub condition: Condition,
    pub temp_max: f64,
    pub temp_min: f64,
    pub precip_prob: i64,
}

#[derive(Debug, Clone, Default)]
pub struct WeatherData {
    pub current_temp: f64,
    pub current_condition: Option<Condition>,
    pub current_precip_prob: i64,
    /// Days 0..=5; index 0 is today.
    pub days: Vec<DayForecast>,
}

pub struct WeatherCache {
    data: Arc<RwLock<Option<WeatherData>>>,
    last_fetch: Arc<RwLock<Option<Instant>>>,
    latitude: f64,
    longitude: f64,
    timezone: String,
}

const REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);

impl WeatherCache {
    pub fn from_config(config: &config::Config) -> Result<Self> {
        let latitude: f64 = config.get_float("weather.latitude")?;
        let longitude: f64 = config.get_float("weather.longitude")?;
        let timezone: String = config
            .get_str("weather.timezone")
            .unwrap_or_else(|_| "auto".to_string());
        Ok(Self {
            data: Arc::new(RwLock::new(None)),
            last_fetch: Arc::new(RwLock::new(None)),
            latitude,
            longitude,
            timezone,
        })
    }

    /// Returns cached data, triggering a background refresh when stale.
    /// Never blocks on the network — first call returns None while the
    /// initial fetch is in flight.
    pub fn get(&self) -> Option<WeatherData> {
        let stale = match *self.last_fetch.read().ok()? {
            Some(t) => t.elapsed() > REFRESH_INTERVAL,
            None => true,
        };
        if stale {
            // Mark fetch time FIRST so concurrent get() calls don't spawn
            // duplicate fetches while one is in flight.
            if let Ok(mut t) = self.last_fetch.write() {
                *t = Some(Instant::now());
            }
            let data_slot = Arc::clone(&self.data);
            let lat = self.latitude;
            let lon = self.longitude;
            let tz = self.timezone.clone();
            tokio::spawn(async move {
                // Fetch on a blocking thread — ureq is synchronous.
                let result =
                    tokio::task::spawn_blocking(move || Self::fetch_coords(lat, lon, &tz)).await;
                match result {
                    Ok(Ok(data)) => {
                        if let Ok(mut slot) = data_slot.write() {
                            *slot = Some(data);
                        }
                    }
                    Ok(Err(e)) => log::warn!("weather: fetch failed: {}", e),
                    Err(e) => log::warn!("weather: fetch task failed: {}", e),
                }
            });
        }
        self.data.read().ok().and_then(|d| d.clone())
    }

    fn fetch_coords(latitude: f64, longitude: f64, timezone: &str) -> Result<WeatherData> {
        let url = format!(
            "https://api.open-meteo.com/v1/forecast?latitude={:.5}&longitude={:.5}\
             &current=temperature_2m,weather_code,precipitation_probability\
             &daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max\
             &timezone={}&forecast_days=6",
            latitude,
            longitude,
            urlencode(timezone)
        );

        let body = ureq::get(&url)
            .timeout(Duration::from_secs(10))
            .call()?
            .into_string()?;
        let json: Value = serde_json::from_str(&body)?;

        let mut out = WeatherData::default();

        if let Some(cur) = json.get("current") {
            out.current_temp = cur["temperature_2m"].as_f64().unwrap_or(0.0);
            out.current_precip_prob = cur["precipitation_probability"].as_i64().unwrap_or(0);
            out.current_condition = cur["weather_code"].as_i64().map(Condition::from_wmo);
        }

        if let Some(daily) = json.get("daily") {
            let times = daily["time"].as_array();
            let codes = daily["weather_code"].as_array();
            let maxs = daily["temperature_2m_max"].as_array();
            let mins = daily["temperature_2m_min"].as_array();
            let probs = daily["precipitation_probability_max"].as_array();

            if let (Some(times), Some(codes), Some(maxs), Some(mins), Some(probs)) =
                (times, codes, maxs, mins, probs)
            {
                for i in 0..times.len() {
                    out.days.push(DayForecast {
                        condition: codes[i]
                            .as_i64()
                            .map(Condition::from_wmo)
                            .unwrap_or(Condition::Overcast),
                        temp_max: maxs[i].as_f64().unwrap_or(0.0),
                        temp_min: mins[i].as_f64().unwrap_or(0.0),
                        precip_prob: probs[i].as_i64().unwrap_or(0),
                    });
                }
            }
        }

        Ok(out)
    }
}

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
