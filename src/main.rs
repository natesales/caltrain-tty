mod caltrain;
mod glass;
mod page;

use std::collections::HashSet;
use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use caltrain::Board;
use page::{Phase, View};

const HELP: &str = "\
caltrain-tty — Redwood City departures on a glass teletype

Usage:
  caltrain-tty [--stop ID]... [--once]

  --stop ID   platform to list; repeat for each stop
  --once      print one page and exit

The page is the CRT's 44-column board. Times are Pacific.
With no --stop, the board lists Redwood City: 70141 and 70142.
";

#[tokio::main]
async fn main() {
    let run = match parse_args_from(std::env::args().skip(1)) {
        Ok(Some(run)) => run,
        Ok(None) => {
            print!("{HELP}");
            return;
        }
        Err(err) => {
            eprintln!("caltrain-tty: {err}\n");
            eprint!("{HELP}");
            std::process::exit(2);
        }
    };
    if run.once || !io::stdout().is_terminal() {
        let code = print_once(&run.stops).await;
        std::process::exit(code);
    }
    live(run.stops).await;
}

struct Run {
    once: bool,
    stops: Vec<String>,
}

fn parse_args_from<I, S>(args: I) -> Result<Option<Run>, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut once = false;
    let mut stops = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_ref() {
            "--once" => once = true,
            "--help" | "-h" => return Ok(None),
            "--stop" => {
                let Some(id) = args.next() else {
                    return Err("--stop needs a stop id".to_string());
                };
                stops.push(stop_id(id.as_ref())?);
            }
            other if let Some(id) = other.strip_prefix("--stop=") => {
                stops.push(stop_id(id)?);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if stops.is_empty() {
        stops = caltrain::DEFAULT_STOPS
            .iter()
            .map(|id| (*id).to_string())
            .collect();
    }
    Ok(Some(Run { once, stops }))
}

fn stop_id(id: &str) -> Result<String, String> {
    if id.is_empty() || id.starts_with('-') {
        return Err(format!("--stop needs a stop id, got {id}"));
    }
    Ok(id.to_string())
}

async fn print_once(stops: &[String]) -> i32 {
    let frame = glass::Frame::detect();
    match caltrain::fetch_board(stops.to_vec()).await {
        Ok(board) => {
            let now = caltrain::pacific_now(board.offset_ms);
            let page = page::render(&View {
                now,
                trains: &board.trains,
                station: &board.station,
                feed_at: Some(now),
                phase: Phase::Live,
                error: None,
                width: frame.width,
                max_trains: frame.max_trains,
                double_space: frame.double_space,
            });
            for line in page.lines {
                println!("{line}");
            }
            0
        }
        Err(err) => {
            let now = caltrain::pacific_now(0);
            let page = page::render(&View {
                now,
                trains: &[],
                station: "",
                feed_at: None,
                phase: Phase::Trouble,
                error: Some(&err),
                width: frame.width,
                max_trains: frame.max_trains,
                double_space: false,
            });
            for line in page.lines {
                println!("{line}");
            }
            1
        }
    }
}

async fn live(stops: Vec<String>) {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = write!(io::stdout(), "\x1b[0m\x1b[999;1H\n");
        let _ = io::stdout().flush();
        default_hook(info);
    }));

    tokio::spawn(async {
        watch_signals().await;
        glass::request_stop();
    });

    let mut glass = match glass::Glass::open() {
        Ok(glass) => glass,
        Err(err) => {
            eprintln!("caltrain-tty: {err}");
            std::process::exit(1);
        }
    };

    let mut board: Option<Board> = None;
    let mut phase = Phase::Requesting;
    let mut error: Option<String> = None;
    let mut feed_at = None;
    let mut fetch: Option<tokio::task::JoinHandle<Result<Board, String>>> = None;
    let mut next_fetch = Instant::now();
    let mut trip_ids: Vec<String> = Vec::new();
    let mut shown_live = false;
    let mut announced = HashSet::new();

    loop {
        if glass::stop_requested() {
            break;
        }
        glass.refit();

        if fetch.as_ref().is_some_and(|handle| handle.is_finished()) {
            let handle = fetch.take().expect("finished fetch");
            match handle.await {
                Ok(Ok(got)) => {
                    feed_at = Some(caltrain::pacific_now(got.offset_ms));
                    board = Some(got);
                    phase = Phase::Live;
                    error = None;
                    next_fetch = Instant::now() + Duration::from_secs(30);
                }
                Ok(Err(err)) => {
                    phase = Phase::Trouble;
                    error = Some(err);
                    next_fetch = Instant::now() + Duration::from_secs(15);
                }
                Err(err) => {
                    phase = Phase::Trouble;
                    error = Some(err.to_string());
                    next_fetch = Instant::now() + Duration::from_secs(15);
                }
            }
        } else if fetch.is_none() && Instant::now() >= next_fetch {
            let stops = stops.clone();
            fetch = Some(tokio::spawn(caltrain::fetch_board(stops)));
        }

        let offset = board.as_ref().map(|item| item.offset_ms).unwrap_or(0);
        let now = caltrain::pacific_now(offset);
        let trains = board
            .as_ref()
            .map(|item| item.trains.as_slice())
            .unwrap_or(&[]);
        let err = error.as_deref();
        let station = board
            .as_ref()
            .map(|item| item.station.as_str())
            .unwrap_or("");
        let rendered = page::render(&View {
            now,
            trains,
            station,
            feed_at,
            phase,
            error: err,
            width: glass.frame.width,
            max_trains: glass.frame.max_trains,
            double_space: glass.frame.double_space,
        });

        let structural = rendered.trip_ids != trip_ids;
        let body = rendered.body.clone();
        // The first schedule appears at once. Later changes to the train list scan.
        glass.present(&rendered.lines, body, shown_live && structural);
        if phase == Phase::Live {
            shown_live = true;
        }
        trip_ids = rendered.trip_ids;

        announced.retain(|id| trip_ids.iter().any(|trip| trip == id));
        if rendered.now_ids.iter().any(|id| !announced.contains(id)) {
            print!("\x07");
            let _ = io::stdout().flush();
        }
        announced.extend(rendered.now_ids.iter().cloned());

        if glass::stop_requested() {
            break;
        }
        for _ in 0..10 {
            if glass::stop_requested() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    glass.shutdown();
}

async fn watch_signals() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(term) => term,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_stops_are_redwood_city() {
        let run = parse_args_from(Vec::<&str>::new()).unwrap().unwrap();
        assert!(!run.once);
        assert_eq!(run.stops, ["70141", "70142"]);
    }

    #[test]
    fn stop_flags_replace_the_default() {
        let run = parse_args_from(["--stop", "70011", "--stop=70012", "--once"])
            .unwrap()
            .unwrap();
        assert!(run.once);
        assert_eq!(run.stops, ["70011", "70012"]);
    }

    #[test]
    fn stop_flag_requires_an_id() {
        assert!(parse_args_from(["--stop"]).is_err());
        assert!(parse_args_from(["--stop", "--once"]).is_err());
        assert!(parse_args_from(["--stop="]).is_err());
    }
}
