use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use chrono_tz::America::Los_Angeles;
use chrono_tz::Tz;
use serde::Deserialize;
use zip::ZipArchive;

/// Live GTFS-Realtime trip updates published by Caltrain. No API key.
const TRIP_UPDATES_URL: &str = "https://www.caltrain.com/files/rt/tripupdates/CT.json";
/// Static GTFS linked from Caltrain's developer page. Used for trip destinations.
const GTFS_URL: &str = "https://data.trilliumtransit.com/gtfs/caltrain-ca-us/caltrain-ca-us.zip";

/// Northbound and southbound platforms at Redwood City.
pub const DEFAULT_STOPS: &[&str] = &["70141", "70142"];

const GTFS_TTL: Duration = Duration::from_secs(6 * 60 * 60);

pub type Pacific = DateTime<Tz>;

#[derive(Debug, Clone)]
pub struct Train {
    pub trip_id: String,
    pub service: String,
    pub destination: String,
    pub departure_ms: i64,
}

pub struct Board {
    pub trains: Vec<Train>,
    /// Place name for the first requested stop, such as `22ND STREET`.
    pub station: String,
    /// `server_time - local_time`, so a clock without NTP still counts down.
    pub offset_ms: i64,
}

struct GtfsBundle {
    headsigns: HashMap<String, String>,
    /// Stop id, including a parent station id, to the short place name.
    stations: HashMap<String, String>,
}

struct GtfsCache {
    fetched_at: Instant,
    bundle: Arc<GtfsBundle>,
}

static GTFS_CACHE: Mutex<Option<GtfsCache>> = Mutex::new(None);

pub fn pacific_now(offset_ms: i64) -> Pacific {
    let local_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    DateTime::<Utc>::from_timestamp_millis(local_ms + offset_ms)
        .unwrap_or_else(Utc::now)
        .with_timezone(&Los_Angeles)
}

pub async fn fetch_board(stops: Vec<String>) -> Result<Board, String> {
    tokio::task::spawn_blocking(move || fetch_board_blocking(&stops))
        .await
        .map_err(|err| format!("fetch task: {err}"))?
}

fn fetch_board_blocking(stops: &[String]) -> Result<Board, String> {
    let mut last_err = String::from("trip updates failed");
    for attempt in 0..3 {
        match fetch_once(stops) {
            Ok(board) => return Ok(board),
            Err(err) if attempt < 2 => {
                last_err = err;
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(err) => return Err(err),
        }
    }
    Err(last_err)
}

fn fetch_once(stops: &[String]) -> Result<Board, String> {
    let feed = fetch_feed()?;
    // A missing schedule still leaves a usable board: direction picks the terminus.
    let gtfs = gtfs().ok();
    let headsigns = gtfs.as_ref().map(|bundle| &bundle.headsigns);
    let station = stops
        .first()
        .and_then(|id| gtfs.as_ref().and_then(|bundle| bundle.stations.get(id).cloned()))
        .unwrap_or_default();

    let local_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let server_ms = if feed.header.timestamp > 0 {
        feed.header.timestamp * 1000
    } else {
        local_ms
    };

    let mut trains = Vec::new();
    for entity in feed.entities {
        let Some(update) = entity.trip_update else {
            continue;
        };
        let Some(stop) = update
            .stop_time_update
            .iter()
            .find(|stop| stops.iter().any(|id| id == &stop.stop_id))
        else {
            continue;
        };

        let arrival_s = stop.arrival.as_ref().and_then(|event| event.time);
        let departure_s = stop.departure.as_ref().and_then(|event| event.time);
        let departure_s = match (arrival_s, departure_s) {
            (_, Some(departure)) => departure,
            (Some(arrival), None) => arrival,
            (None, None) => continue,
        };

        let direction = direction_label(update.trip.direction_id, &stop.stop_id);
        let destination = headsigns
            .as_ref()
            .and_then(|map| map.get(&update.trip.trip_id))
            .map(|sign| ascii_upper(sign))
            .filter(|sign| !sign.is_empty())
            .unwrap_or_else(|| fallback_destination(&direction));
        trains.push(Train {
            trip_id: update.trip.trip_id,
            service: service_name(&update.trip.route_id),
            destination,
            departure_ms: departure_s * 1000,
        });
    }

    Ok(Board {
        trains,
        station,
        offset_ms: server_ms - local_ms,
    })
}

fn http_get(url: &str) -> Result<Vec<u8>, String> {
    let response = http_client()?
        .get(url)
        .send()
        .and_then(|response| response.error_for_status())
        .map_err(|err| format!("http: {err}"))?;
    response
        .bytes()
        .map(|bytes| bytes.to_vec())
        .map_err(|err| format!("http: {err}"))
}

fn http_client() -> Result<&'static reqwest::blocking::Client, String> {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("caltrain-tty/0.1 (redwood-city)")
        .build()
        .map_err(|err| format!("http: {err}"))?;
    Ok(CLIENT.get_or_init(|| client))
}

fn fetch_feed() -> Result<Feed, String> {
    let bytes = http_get(TRIP_UPDATES_URL)?;
    serde_json::from_slice(&bytes).map_err(|err| format!("trip updates: {err}"))
}

fn gtfs() -> Result<Arc<GtfsBundle>, String> {
    {
        let guard = GTFS_CACHE.lock().map_err(|err| err.to_string())?;
        if let Some(cache) = guard.as_ref() {
            if cache.fetched_at.elapsed() < GTFS_TTL {
                return Ok(Arc::clone(&cache.bundle));
            }
        }
    }

    let bytes = http_get(GTFS_URL).map_err(|err| format!("gtfs download: {err}"))?;

    let bundle = Arc::new(parse_gtfs(&bytes)?);
    let mut guard = GTFS_CACHE.lock().map_err(|err| err.to_string())?;
    *guard = Some(GtfsCache {
        fetched_at: Instant::now(),
        bundle: Arc::clone(&bundle),
    });
    Ok(bundle)
}

fn parse_gtfs(zip_bytes: &[u8]) -> Result<GtfsBundle, String> {
    Ok(GtfsBundle {
        headsigns: parse_headsigns(zip_bytes)?,
        stations: parse_stations(zip_bytes).unwrap_or_default(),
    })
}

fn parse_headsigns(zip_bytes: &[u8]) -> Result<HashMap<String, String>, String> {
    let mut archive =
        ZipArchive::new(Cursor::new(zip_bytes)).map_err(|err| format!("gtfs zip: {err}"))?;
    let mut file = archive
        .by_name("trips.txt")
        .map_err(|err| format!("gtfs trips.txt: {err}"))?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|err| format!("gtfs trips.txt: {err}"))?;

    let mut reader = csv::Reader::from_reader(text.as_bytes());
    let headers = reader
        .headers()
        .map_err(|err| format!("gtfs trips.txt: {err}"))?
        .clone();
    let trip_idx = headers
        .iter()
        .position(|h| h == "trip_id")
        .ok_or("gtfs trips.txt missing trip_id")?;
    let headsign_idx = headers
        .iter()
        .position(|h| h == "trip_headsign")
        .ok_or("gtfs trips.txt missing trip_headsign")?;

    let mut map = HashMap::new();
    for record in reader.records() {
        let record = record.map_err(|err| format!("gtfs trips.txt: {err}"))?;
        let Some(trip_id) = record.get(trip_idx) else {
            continue;
        };
        let Some(headsign) = record.get(headsign_idx) else {
            continue;
        };
        if !trip_id.is_empty() && !headsign.is_empty() {
            map.insert(trip_id.to_string(), headsign.to_string());
        }
    }
    if map.is_empty() {
        return Err("gtfs trips.txt had no trips".into());
    }
    Ok(map)
}

fn parse_stations(zip_bytes: &[u8]) -> Result<HashMap<String, String>, String> {
    let mut archive =
        ZipArchive::new(Cursor::new(zip_bytes)).map_err(|err| format!("gtfs zip: {err}"))?;
    let mut file = archive
        .by_name("stops.txt")
        .map_err(|err| format!("gtfs stops.txt: {err}"))?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|err| format!("gtfs stops.txt: {err}"))?;

    let mut reader = csv::Reader::from_reader(text.as_bytes());
    let headers = reader
        .headers()
        .map_err(|err| format!("gtfs stops.txt: {err}"))?
        .clone();
    let id_idx = headers
        .iter()
        .position(|h| h == "stop_id")
        .ok_or("gtfs stops.txt missing stop_id")?;
    let name_idx = headers
        .iter()
        .position(|h| h == "stop_name")
        .ok_or("gtfs stops.txt missing stop_name")?;
    let parent_idx = headers.iter().position(|h| h == "parent_station");

    let mut raw = HashMap::new();
    for record in reader.records() {
        let record = record.map_err(|err| format!("gtfs stops.txt: {err}"))?;
        let Some(id) = record.get(id_idx) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let name = record.get(name_idx).unwrap_or("").to_string();
        let parent = parent_idx
            .and_then(|idx| record.get(idx))
            .unwrap_or("")
            .to_string();
        raw.insert(id.to_string(), (name, parent));
    }

    let mut labels = HashMap::new();
    for id in raw.keys() {
        let label = station_label(&raw, id);
        if !label.is_empty() {
            labels.insert(id.clone(), label);
        }
    }
    Ok(labels)
}

/// Parent station name when the stop is a platform, then the short place name.
fn station_label(raw: &HashMap<String, (String, String)>, id: &str) -> String {
    let Some((name, parent)) = raw.get(id) else {
        return String::new();
    };
    let source = if parent.is_empty() {
        name.as_str()
    } else {
        raw.get(parent)
            .map(|(name, _)| name.as_str())
            .unwrap_or(name.as_str())
    };
    shorten_station(source)
}

fn shorten_station(name: &str) -> String {
    let upper = ascii_upper(name);
    for suffix in [
        " CALTRAIN NORTHBOUND",
        " CALTRAIN SOUTHBOUND",
        " CALTRAIN STATION",
        " STATION",
    ] {
        if let Some(trimmed) = upper.strip_suffix(suffix) {
            let trimmed = trimmed.trim_end();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    upper
}

fn service_name(route_id: &str) -> String {
    let lower = route_id.to_ascii_lowercase();
    let name = if lower.contains("express") {
        "EXP"
    } else if lower.contains("limited") {
        "LTD"
    } else if lower.contains("south") {
        "SCT"
    } else if lower.contains("local") || route_id.is_empty() {
        "LOC"
    } else {
        return ascii_upper(route_id).chars().take(3).collect();
    };
    name.to_string()
}

fn direction_label(direction_id: Option<i64>, stop_id: &str) -> String {
    match direction_id {
        Some(0) => "N".to_string(),
        Some(1) => "S".to_string(),
        _ if stop_id.ends_with('1') => "N".to_string(),
        _ => "S".to_string(),
    }
}

fn fallback_destination(direction: &str) -> String {
    if direction == "N" {
        "SAN FRANCISCO".to_string()
    } else {
        "SAN JOSE".to_string()
    }
}

pub fn ascii_upper(value: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for ch in value.chars() {
        if ch.is_ascii_whitespace() {
            gap = !out.is_empty();
            continue;
        }
        if !ch.is_ascii() {
            continue;
        }
        if gap {
            out.push(' ');
            gap = false;
        }
        out.push(ch.to_ascii_uppercase());
    }
    out
}

#[derive(Debug, Deserialize)]
struct Feed {
    #[serde(rename = "Header")]
    header: FeedHeader,
    #[serde(rename = "Entities", default)]
    entities: Vec<Entity>,
}

#[derive(Debug, Deserialize)]
struct FeedHeader {
    #[serde(rename = "Timestamp", default)]
    timestamp: i64,
}

#[derive(Debug, Deserialize)]
struct Entity {
    #[serde(rename = "TripUpdate")]
    trip_update: Option<TripUpdate>,
}

#[derive(Debug, Deserialize)]
struct TripUpdate {
    #[serde(rename = "Trip")]
    trip: Trip,
    #[serde(rename = "StopTimeUpdate", default)]
    stop_time_update: Vec<StopTimeUpdate>,
}

#[derive(Debug, Deserialize)]
struct Trip {
    #[serde(rename = "TripId")]
    trip_id: String,
    #[serde(rename = "RouteId", default)]
    route_id: String,
    #[serde(rename = "DirectionId")]
    direction_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct StopTimeUpdate {
    #[serde(rename = "StopId")]
    stop_id: String,
    #[serde(rename = "Arrival")]
    arrival: Option<TimedEvent>,
    #[serde(rename = "Departure")]
    departure: Option<TimedEvent>,
}

#[derive(Debug, Deserialize)]
struct TimedEvent {
    #[serde(rename = "Time")]
    time: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_and_direction_names() {
        assert_eq!(service_name("Local"), "LOC");
        assert_eq!(service_name("Express"), "EXP");
        assert_eq!(service_name("Limited"), "LTD");
        assert_eq!(service_name("South_County"), "SCT");
        assert_eq!(service_name(""), "LOC");
        assert_eq!(direction_label(Some(0), "70141"), "N");
        assert_eq!(direction_label(Some(1), "70142"), "S");
        assert_eq!(direction_label(None, "70141"), "N");
        assert_eq!(direction_label(None, "70142"), "S");
    }

    #[test]
    fn ascii_upper_collapses_space() {
        assert_eq!(ascii_upper("  San   Francisco "), "SAN FRANCISCO");
    }

    #[test]
    fn station_names_drop_the_platform_words() {
        assert_eq!(shorten_station("Redwood City Station"), "REDWOOD CITY");
        assert_eq!(shorten_station("22nd Street Station"), "22ND STREET");
        assert_eq!(
            shorten_station("San Francisco Caltrain Station"),
            "SAN FRANCISCO"
        );
        assert_eq!(
            shorten_station("22nd Street Caltrain Northbound"),
            "22ND STREET"
        );
    }
}
