use std::collections::HashSet;
use std::ops::Range;

use chrono::{Datelike, Timelike};

use crate::caltrain::{self, Pacific, Train};

/// How many fixed rows sit outside the train list: masthead, clock, rule,
/// column header, closing rule, status.
const CHROME: usize = 6;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Requesting,
    Live,
    Trouble,
}

pub struct View<'a> {
    pub now: Pacific,
    pub trains: &'a [Train],
    pub station: &'a str,
    pub feed_at: Option<Pacific>,
    pub phase: Phase,
    pub error: Option<&'a str>,
    pub width: usize,
    pub max_trains: usize,
    pub double_space: bool,
}

pub struct Page {
    pub lines: Vec<String>,
    /// Trip ids on the page, in departure order. Used to detect a new report.
    pub trip_ids: Vec<String>,
    /// Trips whose departure is due now.
    pub now_ids: Vec<String>,
    /// Train rows, used to check the rendered list.
    pub body: Range<usize>,
}

/// Double-space only on a tall screen. A short CRT keeps one line per train
/// so the large glyphs are not separated by empty rows.
pub fn capacity(line_budget: usize) -> (usize, bool) {
    let room = line_budget.saturating_sub(CHROME);
    if room >= 16 {
        let mut count = 8;
        while count > 0 && train_block_lines(count, true) > room {
            count -= 1;
        }
        (count.max(1), count > 1)
    } else {
        (room.clamp(1, 8), false)
    }
}

fn train_block_lines(count: usize, double_space: bool) -> usize {
    if count == 0 {
        return 1;
    }
    if double_space {
        count + count.saturating_sub(1)
    } else {
        count
    }
}

pub fn render(view: &View<'_>) -> Page {
    let width = view.width.max(1);
    let shown = select(view.trains, view.now.timestamp_millis(), view.max_trains);

    let mut lines = Vec::new();
    lines.push(masthead(width, view.station));
    lines.push(space_between(
        &date_stamp(&view.now),
        &clock_stamp(&view.now),
        width,
    ));
    lines.push(rule(width));
    lines.push(train_row(width, "SVC", "DESTINATION", "DEPARTURE", "MIN"));

    let body_start = lines.len();
    if shown.is_empty() {
        lines.push(match view.phase {
            Phase::Requesting => center("STAND BY", width),
            Phase::Live => center("NO TRAINS LISTED", width),
            Phase::Trouble => center("NO CARRIER", width),
        });
        if view.phase == Phase::Trouble {
            if let Some(err) = view.error {
                let text = caltrain::ascii_upper(err);
                if !text.is_empty() {
                    lines.push(fit(&text, width));
                }
            }
        }
    } else {
        for (index, train) in shown.iter().enumerate() {
            if view.double_space && index > 0 {
                lines.push(" ".repeat(width));
            }
            let minutes = minutes_until(train.departure_ms, view.now.timestamp_millis());
            lines.push(train_row(
                width,
                &train.service,
                &train.destination,
                &clock_face(train.departure_ms),
                &due_label(minutes),
            ));
        }
    }
    let body_end = lines.len();

    lines.push(rule(width));
    lines.push(status_line(view, width, !shown.is_empty()));

    let lines = lines
        .into_iter()
        .map(|line| pad_line(line, width))
        .collect();
    let now_ids = shown
        .iter()
        .filter(|train| minutes_until(train.departure_ms, view.now.timestamp_millis()) <= 0)
        .map(|train| train.trip_id.clone())
        .collect();
    let trip_ids = shown.into_iter().map(|train| train.trip_id).collect();

    Page {
        lines,
        trip_ids,
        now_ids,
        body: body_start..body_end,
    }
}

fn select(trains: &[Train], now_ms: i64, limit: usize) -> Vec<Train> {
    let mut rows: Vec<Train> = trains
        .iter()
        .filter(|train| {
            let due = minutes_until(train.departure_ms, now_ms);
            (-1..=180).contains(&due)
        })
        .cloned()
        .collect();
    rows.sort_by(|a, b| {
        a.departure_ms
            .cmp(&b.departure_ms)
            .then(a.trip_id.cmp(&b.trip_id))
    });
    let mut seen = HashSet::new();
    rows.retain(|train| seen.insert(train.trip_id.clone()));
    rows.truncate(limit);
    rows
}

pub fn minutes_until(departure_ms: i64, now_ms: i64) -> i64 {
    (departure_ms - now_ms) / 60_000
}

fn due_label(minutes: i64) -> String {
    if minutes <= 0 {
        "NOW".to_string()
    } else if minutes > 999 {
        "999".to_string()
    } else {
        format!("{minutes}")
    }
}

fn masthead(width: usize, station: &str) -> String {
    let left = "CALTRAIN DEPARTURES";
    let right = station;
    if left.len() + 1 + right.len() <= width {
        space_between(left, right, width)
    } else {
        space_between("CALTRAIN", right, width)
    }
}

fn status_line(view: &View<'_>, width: usize, has_trains: bool) -> String {
    let left = match view.phase {
        Phase::Requesting => "REQUESTING",
        Phase::Live => "CARRIER OK",
        Phase::Trouble if has_trains => "LINE TROUBLE",
        Phase::Trouble => "NO CARRIER",
    };
    let right = view
        .feed_at
        .map(|stamp| format!("FEED {}", short_clock(&stamp)))
        .unwrap_or_default();
    space_between(left, &right, width)
}

fn date_stamp(now: &Pacific) -> String {
    const DAYS: [&str; 7] = ["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"];
    const MONTHS: [&str; 12] = [
        "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
    ];
    let day = DAYS[now.weekday().num_days_from_monday() as usize];
    let month = MONTHS[now.month0() as usize];
    format!("{day} {:02} {month} {}", now.day(), now.year())
}

fn clock_stamp(now: &Pacific) -> String {
    let (pm, hour) = now.hour12();
    let suffix = if pm { "PM" } else { "AM" };
    format!("{hour:02}:{:02}:{:02} {suffix}", now.minute(), now.second())
}

fn short_clock(now: &Pacific) -> String {
    let (pm, hour) = now.hour12();
    let suffix = if pm { "PM" } else { "AM" };
    format!("{hour:02}:{:02} {suffix}", now.minute())
}

/// Clock time like "10:09 PM" or " 9:05 AM", always 8 columns.
fn clock_face(unix_ms: i64) -> String {
    let Some(dt) = chrono::DateTime::from_timestamp_millis(unix_ms) else {
        return "--:-- --".to_string();
    };
    let local = dt.with_timezone(&chrono_tz::America::Los_Angeles);
    let (pm, hour) = local.hour12();
    let suffix = if pm { "PM" } else { "AM" };
    format!("{hour:>2}:{:02} {suffix}", local.minute())
}

/// The service abbreviation of a non-local train, drawn in reverse video.
/// Local (`LOC`) and the column header stay in the normal face.
pub fn reverse_span(line: &str) -> Option<Range<usize>> {
    let service = line.get(..3)?;
    if service == "LOC" || service == "SVC" {
        return None;
    }
    if !service.bytes().all(|byte| byte.is_ascii_uppercase()) {
        return None;
    }
    if line.as_bytes().get(3..5) != Some(b"  ") {
        return None;
    }
    Some(0..3)
}

/// One layout for the CRT and for local runs: service, destination, departure, minutes.
fn train_row(width: usize, service: &str, dest: &str, departure: &str, due: &str) -> String {
    const SVC_W: usize = 3;
    const DEP_W: usize = 9;
    const MIN_W: usize = 3;
    let dest_w = width.saturating_sub(SVC_W + DEP_W + MIN_W + 6).max(1);
    let parts = [
        fit_field(service, SVC_W, false),
        fit_field(dest, dest_w, false),
        fit_field(departure, DEP_W, false),
        fit_field(due, MIN_W, true),
    ];
    pad_line(parts.join("  "), width)
}

fn fit_field(text: &str, width: usize, right: bool) -> String {
    let text = fit(text, width);
    let pad = width.saturating_sub(text.chars().count());
    if right {
        format!("{}{text}", " ".repeat(pad))
    } else {
        format!("{text}{}", " ".repeat(pad))
    }
}

fn rule(width: usize) -> String {
    "-".repeat(width)
}

fn space_between(left: &str, right: &str, width: usize) -> String {
    let left = fit(left, width);
    let remain = width.saturating_sub(left.len());
    if remain == 0 {
        return left;
    }
    let right = fit(right, remain.saturating_sub(1));
    let gap = width - left.len() - right.len();
    format!("{left}{}{right}", " ".repeat(gap))
}

fn center(text: &str, width: usize) -> String {
    let text = fit(text, width);
    let pad = (width - text.len()) / 2;
    format!(
        "{}{}{}",
        " ".repeat(pad),
        text,
        " ".repeat(width - pad - text.len())
    )
}

fn fit(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

fn pad_line(mut line: String, width: usize) -> String {
    if line.chars().count() > width {
        line = fit(&line, width);
    }
    let extra = width.saturating_sub(line.chars().count());
    line.push_str(&" ".repeat(extra));
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use chrono_tz::America::Los_Angeles;

    fn at(ms: i64) -> Pacific {
        Los_Angeles.timestamp_millis_opt(ms).unwrap()
    }

    fn train(id: &str, service: &str, dest: &str, departure_ms: i64) -> Train {
        Train {
            trip_id: id.to_string(),
            service: service.to_string(),
            destination: dest.to_string(),
            departure_ms,
        }
    }

    #[test]
    fn departure_column_lines_up() {
        let header = train_row(44, "SVC", "DESTINATION", "DEPARTURE", "MIN");
        let row = train_row(44, "LOC", "SAN FRANCISCO", "10:09 PM", "12");
        let due = train_row(44, "EXP", "SAN JOSE DIRIDON", "10:21 PM", "NOW");
        assert_eq!(header.len(), 44, "{header:?}");
        assert_eq!(row.len(), 44, "{row:?}");
        assert_eq!(due.len(), 44, "{due:?}");
        assert!(row.starts_with("LOC"), "{row:?}");
        assert!(row.contains("SAN FRANCISCO"), "{row:?}");
        assert!(!row.contains("LOCAL"), "{row:?}");
        let depart = header.find("DEPARTURE").unwrap();
        assert_eq!(&row[depart..depart + 8], "10:09 PM");
        assert!(header.trim_end().ends_with("MIN"));
        assert!(row.trim_end().ends_with("12"), "{row:?}");
        assert!(due.trim_end().ends_with("NOW"), "{due:?}");
        assert_eq!(reverse_span(&header), None);
        assert_eq!(reverse_span(&row), None);
        assert_eq!(reverse_span(&due), Some(0..3));
        assert_eq!(reverse_span(&train_row(44, "LTD", "TAMIEN", "7:30 PM", "35")), Some(0..3));
        assert_eq!(reverse_span(&train_row(44, "SCT", "GILROY", "8:01 PM", "40")), Some(0..3));
    }

    #[test]
    fn page_lists_upcoming_trains_and_drops_the_rest() {
        let now = at(1_700_000_000_000);
        let now_ms = now.timestamp_millis();
        let trains = vec![
            train("1", "LOC", "SAN FRANCISCO", now_ms + 12 * 60_000),
            train("2", "EXP", "SAN JOSE", now_ms + 4 * 60_000),
            train("3", "LTD", "SAN FRANCISCO", now_ms - 5 * 60_000),
            train("4", "LOC", "GILROY", now_ms + 200 * 60_000),
            train("1", "LOC", "SAN FRANCISCO", now_ms + 30 * 60_000),
        ];
        let page = render(&View {
            now,
            trains: &trains,
            station: "REDWOOD CITY",
            feed_at: Some(now),
            phase: Phase::Live,
            error: None,
            width: 44,
            max_trains: 8,
            double_space: false,
        });

        assert_eq!(page.trip_ids, vec!["2".to_string(), "1".to_string()]);
        assert!(page.now_ids.is_empty());
        assert!(page.lines.iter().all(|line| line.len() == 44));
        assert!(page.lines[3].contains("DEPARTURE"));
        assert!(page.lines[3].contains("SVC"));
        assert!(page.lines[page.body.start].contains(" 2:17 PM"));
        let body = page.lines[page.body.clone()].join("\n");
        assert!(body.contains("SAN JOSE"));
        assert!(body.contains("SAN FRANCISCO"));
        assert!(body.contains("EXP"));
        assert!(!body.contains("GILROY"));
        assert!(page.lines.last().unwrap().contains("CARRIER OK"));
        assert!(page.lines[0].contains("REDWOOD CITY"));
    }

    #[test]
    fn due_train_is_now() {
        let now = at(1_700_000_000_000);
        let trains = vec![train(
            "9",
            "LOC",
            "SAN FRANCISCO",
            now.timestamp_millis() + 30_000,
        )];
        let page = render(&View {
            now,
            trains: &trains,
            station: "22ND STREET",
            feed_at: None,
            phase: Phase::Live,
            error: None,
            width: 40,
            max_trains: 4,
            double_space: false,
        });
        assert!(page.lines[0].contains("22ND STREET"));
        assert_eq!(page.now_ids, vec!["9".to_string()]);
        assert!(page.lines.iter().any(|line| line.contains("NOW")));
    }

    #[test]
    fn twenty_four_rows_fit_eight_double_spaced_trains() {
        // 1 top margin + 24 content budget is a small CRT with the cursor held
        // on its own row: 24 - 1 - 1 = 22 usable lines.
        assert_eq!(capacity(22), (8, true));
    }
}
