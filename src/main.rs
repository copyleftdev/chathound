//! chathound CLI: scan | events | listen | stats

use chathound::*;
use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

const UA: &str = concat!("chathound/", env!("CARGO_PKG_VERSION"));

fn usage() -> ! {
    eprintln!(
        "chathound v{} — polite, exhaustive Kalshi live-chat tap

USAGE:
  chathound scan   [OUT_DIR=data]          discover every event + probe chat_enabled
                                            -> OUT_DIR/chat_events_DATE.json (run first)
  chathound events [FILE=data/chat_events_*.json]   print chat-enabled events from a scan
  chathound listen [FILE] [OUT.ndjson] [MAX_RPS=1.5]
                                            adaptive tap: hears the whole venue,
                                            writes NDJSON (one record per message)
  chathound stats  [NDJSON]                quick coverage/saturation report

POLITENESS:
  - global token bucket (MAX_RPS sustained, burst 5, +-20% jitter)
  - per-event AIMD: quiet chats age 20s -> 600s, active snap back down
  - HTTP 429/5xx trips a 30s global cooldown; per-event error backoff is
    exponential and capped
  - one request per event per pass, no parallel connections",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(2)
}

fn newest(dir: &str, prefix: &str) -> Option<String> {
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for ent in std::fs::read_dir(dir).ok()? {
        let ent = ent.ok()?;
        let name = ent.file_name().to_string_lossy().to_string();
        if name.starts_with(prefix) && name.ends_with(".json") {
            let m = ent.metadata().ok()?.modified().ok()?;
            if best.as_ref().map(|(t, _)| m > *t).unwrap_or(true) {
                best = Some((m, format!("{dir}/{name}")));
            }
        }
    }
    best.map(|(_, p)| p)
}

fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    // UTC date; good enough for file naming.
    let days = secs / 86_400;
    civil_from_days(days)
}

fn civil_from_days(z: i64) -> String {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[derive(serde::Serialize, serde::Deserialize)]
struct EnabledRec {
    event_ticker: String,
    chat_enabled: bool,
    window_msgs: usize,
    checked_at_utc: String,
}

fn cmd_scan(out_dir: &str) -> Result<(), ChError> {
    std::fs::create_dir_all(out_dir).map_err(|e| ChError::Io(e.to_string()))?;
    println!("discovering events (full venue walk, cursor-paginated)...");
    let tickers = discover_events(UA)?;
    println!(
        "events discovered: {} (probing chat_enabled, ~1 req/event at polite rate)",
        tickers.len()
    );
    let mut rl = RateLimiter::new(1.5);
    let mut recs = Vec::new();
    let mut enabled = 0usize;
    let mut errors = 0usize;
    for t in &tickers {
        rl.acquire();
        match http_get_json(&preview_url(t), UA) {
            Ok(d) => {
                let p: Preview =
                    serde_json::from_value(d).map_err(|e| ChError::Parse(format!("{t}: {e}")))?;
                if p.chat_enabled {
                    enabled += 1;
                }
                recs.push(EnabledRec {
                    event_ticker: t.clone(),
                    chat_enabled: p.chat_enabled,
                    window_msgs: p.messages.len(),
                    checked_at_utc: chrono_free_now(),
                });
            }
            Err(_) => {
                errors += 1;
                recs.push(EnabledRec {
                    event_ticker: t.clone(),
                    chat_enabled: false,
                    window_msgs: 0,
                    checked_at_utc: chrono_free_now(),
                });
            }
        }
        if recs.len() % 500 == 0 {
            println!(
                "  probed {} / {} (enabled {}, errors {})",
                recs.len(),
                tickers.len(),
                enabled,
                errors
            );
        }
    }
    let path = format!("{out_dir}/chat_events_{}.json", today());
    let tmp = format!("{path}.tmp");
    let f = std::fs::File::create(&tmp).map_err(|e| ChError::Io(e.to_string()))?;
    serde_json::to_writer_pretty(f, &recs).map_err(|e| ChError::Io(e.to_string()))?;
    std::fs::rename(&tmp, &path).map_err(|e| ChError::Io(e.to_string()))?;
    println!(
        "scan done: {} events, {} chat-enabled, {} errors -> {path}",
        recs.len(),
        enabled,
        errors
    );
    Ok(())
}

fn chrono_free_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("t{secs}")
}

fn load_enabled(file: &str) -> Result<Vec<String>, ChError> {
    let raw = std::fs::read(file).map_err(|e| ChError::Io(format!("{file}: {e}")))?;
    let recs: Vec<EnabledRec> =
        serde_json::from_slice(&raw).map_err(|e| ChError::Parse(format!("{file}: {e}")))?;
    Ok(recs
        .into_iter()
        .filter(|r| r.chat_enabled)
        .map(|r| r.event_ticker)
        .collect())
}

fn cmd_events(file: &str) -> Result<(), ChError> {
    for t in load_enabled(file)? {
        println!("{t}");
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct OutRec {
    ts_utc: u64,
    event_ticker: String,
    id: String,
    text: String,
    user_id: String,
    user_name: String,
    created_at: String,
    position_side: Option<String>,
    position_label: Option<String>,
    position_cost_usd: Option<f64>,
    position_market: Option<String>,
    spam: bool,
    // Shannon information state of this event's chat so far (see entropy.rs)
    ent_word_bits: f64,
    ent_user_norm: f64,
    ent_side_bits: f64,
}

fn cmd_listen(file: &str, out_path: &str, max_rps: f64) -> Result<(), ChError> {
    let tickers = load_enabled(file)?;
    if tickers.is_empty() {
        return Err(ChError::Io(format!(
            "no chat-enabled events in {file}; run `chathound scan`"
        )));
    }
    println!(
        "listening on {} chat-enabled events @ {max_rps} rps ceiling",
        tickers.len()
    );
    let mut sched = Scheduler::new(tickers);
    let mut rl = RateLimiter::new(max_rps);
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out_path)
        .map_err(|e| ChError::Io(format!("{out_path}: {e}")))?;
    let mut cooldown_until = std::time::Instant::now();
    let mut pass = 0u64;
    let mut ent: HashMap<String, EntropyState> = HashMap::new();
    loop {
        sched.wait_next(Duration::from_secs(60));
        while let Some(t) = sched.pop_due(Duration::from_millis(200)) {
            if std::time::Instant::now() < cooldown_until {
                std::thread::sleep(
                    cooldown_until.saturating_duration_since(std::time::Instant::now()),
                );
            }
            rl.acquire();
            match http_get_json(&preview_url(&t), UA) {
                Ok(d) => match serde_json::from_value::<Preview>(d) {
                    Ok(p) => {
                        if !p.chat_enabled {
                            sched.reschedule(&t, 0, false);
                            continue;
                        }
                        let ids: Vec<String> = p.messages.iter().map(|m| m.id.clone()).collect();
                        let was_primed = sched.primed(&t);
                        let fresh = sched_states_observe(&mut sched, &t, &ids);
                        {
                            let st = ent.entry(t.clone()).or_default();
                            for m in &p.messages {
                                st.update(&m.text, &m.user.name, m.position_side.as_deref());
                            }
                        }
                        let (ent_word_bits, ent_user_norm, ent_side_bits, _, _) = ent
                            .get(&t)
                            .map(EntropyState::report)
                            .unwrap_or((0.0, 0.0, 0.0, 0, 0));
                        for m in &p.messages {
                            let rec = OutRec {
                                ts_utc: unix_now(),
                                event_ticker: t.clone(),
                                id: m.id.clone(),
                                text: m.text.clone(),
                                user_id: m.user.id.clone(),
                                user_name: m.user.name.clone(),
                                created_at: m.created_at.clone(),
                                position_side: m.position_side.clone(),
                                position_label: m.position_label.clone(),
                                position_cost_usd: m.position_cost_usd,
                                position_market: m.position_color_market_ticker.clone(),
                                spam: looks_like_spam(&m.text),
                                ent_word_bits,
                                ent_user_norm,
                                ent_side_bits,
                            };
                            let line = serde_json::to_string(&rec)
                                .map_err(|e| ChError::Io(e.to_string()))?;
                            out.write_all(line.as_bytes())
                                .and_then(|_| out.write_all(b"\n"))
                                .map_err(|e| ChError::Io(e.to_string()))?;
                        }
                        let saturated = was_primed && fresh >= WINDOW_MSGS;
                        if saturated {
                            eprintln!("SATURATED: {t} window churned {fresh} msgs — increase --max-rps to avoid drops");
                        }
                        sched.reschedule(&t, fresh, false);
                    }
                    Err(e) => {
                        eprintln!("parse error {t}: {e}");
                        sched.reschedule(&t, 0, true);
                    }
                },
                Err(e) => {
                    eprintln!("http error {t}: {e}");
                    if let ChError::Http(h) = &e {
                        if h.contains("429") || h.contains("50") {
                            cooldown_until = std::time::Instant::now() + COOLDOWN;
                            eprintln!("server stress -> global {COOLDOWN:?} cooldown");
                        }
                    }
                    sched.reschedule(&t, 0, true);
                }
            }
        }
        pass += 1;
        if pass.is_multiple_of(10) {
            out.flush().ok();
            eprintln!(
                "[pass {pass}] {} events tracked, {} messages seen",
                sched.len(),
                sched.msgs_total()
            );
        }
    }
}

// helper so `observe` stays a pure unit-testable method
fn sched_states_observe(sched: &mut Scheduler, ticker: &str, ids: &[String]) -> usize {
    sched.observe(ticker, ids)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn cmd_stats(ndjson: &str) -> Result<(), ChError> {
    let f = std::fs::File::open(ndjson).map_err(|e| ChError::Io(format!("{ndjson}: {e}")))?;
    let mut per_event: std::collections::HashMap<String, (u64, u64)> = HashMap::new();
    let mut total = 0u64;
    let mut spam = 0u64;
    for line in std::io::BufRead::lines(std::io::BufReader::new(f)) {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        total += 1;
        if v["spam"].as_bool().unwrap_or(false) {
            spam += 1;
        }
        if let Some(t) = v["event_ticker"].as_str() {
            let e = per_event.entry(t.to_string()).or_insert((0, 0));
            e.0 += 1;
            e.1 = e.1.max(v["ts_utc"].as_u64().unwrap_or(0));
        }
    }
    let mut top: Vec<_> = per_event.iter().collect();
    top.sort_by_key(|(_, (n, _))| std::cmp::Reverse(*n));
    println!(
        "records: {total} (spam {spam}) across {} events",
        per_event.len()
    );
    for (t, (n, last)) in top.iter().take(25) {
        println!("  {t:50} {n:6} msgs (last {last})");
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("scan") => {
            let dir = args
                .get(1)
                .map(|s| s.as_str())
                .filter(|s| !s.starts_with('-'))
                .unwrap_or("data");
            cmd_scan(dir).unwrap_or_else(|e| {
                eprintln!("chathound: {e}");
                std::process::exit(1)
            });
        }
        Some("events") => {
            let f = args
                .get(1)
                .map(|s| s.as_str())
                .filter(|s| !s.starts_with('-'))
                .map(|s| s.to_string())
                .or_else(|| newest("data", "chat_events_"))
                .unwrap_or_else(|| usage());
            cmd_events(&f).unwrap_or_else(|e| {
                eprintln!("chathound: {e}");
                std::process::exit(1)
            });
        }
        Some("listen") => {
            let f = args
                .get(1)
                .map(|s| s.as_str())
                .filter(|s| !s.starts_with('-'))
                .map(|s| s.to_string())
                .or_else(|| newest("data", "chat_events_"))
                .unwrap_or_else(|| usage());
            let out = args
                .get(2)
                .map(|s| s.as_str())
                .filter(|s| !s.starts_with('-'))
                .unwrap_or("data/chat_stream.ndjson")
                .to_string();
            let rps = args
                .get(3)
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(1.5);
            cmd_listen(&f, &out, rps).unwrap_or_else(|e| {
                eprintln!("chathound: {e}");
                std::process::exit(1)
            });
        }
        Some("stats") => {
            let f = args
                .get(1)
                .map(|s| s.as_str())
                .filter(|s| !s.starts_with('-'))
                .unwrap_or("data/chat_stream.ndjson");
            cmd_stats(f).unwrap_or_else(|e| {
                eprintln!("chathound: {e}");
                std::process::exit(1)
            });
        }
        _ => usage(),
    }
}
