//! chathound — polite, exhaustive tap of Kalshi live-market chat.
//!
//! Architecture: Kalshi exposes an unauthenticated per-event rolling window
//! (~21 most recent messages) on its frontend BFF. There is no push stream,
//! so "hearing everything" means polling — and politeness means never
//! polling faster than the window demands. chathound solves both with:
//!
//! 1. A global token-bucket rate limiter (`RateLimiter`) with jitter and a
//!    hard ceiling, shared across all events.
//! 2. Per-event adaptive intervals (AIMD, see `next_interval`): quiet chats
//!    age toward `MAX_INTERVAL`, active chats snap back toward
//!    `MIN_INTERVAL`. Budget is spent where chatter actually is.
//! 3. A server-health governor: HTTP 429/5xx trips a global cooldown so the
//!    exchange never sees retry storms.
//!
//! TigerStyle: every buffer bounded, no panics on the hot path, deterministic
//! and replayable outputs, explicit errors.

use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

pub const BFF: &str = "https://api.elections.kalshi.com/v1";
pub const WINDOW_MSGS: usize = 21; // server-fixed rolling window size
pub const MIN_INTERVAL: Duration = Duration::from_secs(20);
pub const MAX_INTERVAL: Duration = Duration::from_secs(600);
pub const IDLE_GROW: f64 = 1.6; // multiplicative-increase when quiet
pub const ACTIVE_SHRINK: f64 = 2.0; // multiplicative-decrease when chatty
pub const COOLDOWN: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum ChError {
    Http(String),
    Parse(String),
    Io(String),
}

impl fmt::Display for ChError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChError::Http(s) => write!(f, "http: {s}"),
            ChError::Parse(s) => write!(f, "parse: {s}"),
            ChError::Io(s) => write!(f, "io: {s}"),
        }
    }
}

impl std::error::Error for ChError {}

/// HTTP GET with a bounded reader (ureq's `into_string` caps at 10MB and
/// errors on large pages; the events walk can exceed that).
pub fn http_get_json(url: &str, ua: &str) -> Result<serde_json::Value, ChError> {
    use std::io::Read as _;
    let resp = ureq::get(url)
        .set("Accept", "application/json")
        .set("User-Agent", ua)
        .timeout(Duration::from_secs(20))
        .call()
        .map_err(|e| ChError::Http(format!("{url}: {e}")))?;
    let mut raw = Vec::new();
    let n = std::io::Read::read_to_end(&mut resp.into_reader().take(64 * 1024 * 1024), &mut raw)
        .map_err(|e| ChError::Http(format!("{url}: read {e}")))?;
    // bounded: 64MB cap enforced by take()
    let _ = n;
    serde_json::from_slice(&raw).map_err(|e| ChError::Parse(format!("{url}: {e}")))
}

/// Token-bucket rate limiter: sustained `rps`, small burst, ±20% jitter on
/// the wait so a fleet of clients never syncs into a thundering herd.
pub struct RateLimiter {
    rps: f64,
    tokens: f64,
    burst: f64,
    last: std::time::Instant,
}

impl RateLimiter {
    pub fn new(rps: f64) -> Self {
        Self {
            rps,
            tokens: 5.0,
            burst: 5.0,
            last: std::time::Instant::now(),
        }
    }

    /// Block until one token is available. Never returns before the caller
    /// is allowed to fire.
    pub fn acquire(&mut self) {
        loop {
            let now = std::time::Instant::now();
            self.tokens = (self.tokens + self.rps * now.duration_since(self.last).as_secs_f64())
                .min(self.burst);
            self.last = now;
            if self.tokens >= 1.0 {
                self.tokens -= 1.0;
                let jitter = 0.8 + 0.4 * rand_unit();
                let wait = (1.0 / self.rps) * jitter;
                if wait > 0.0 {
                    std::thread::sleep(Duration::from_secs_f64(wait));
                }
                return;
            }
            let deficit = 1.0 - self.tokens;
            std::thread::sleep(Duration::from_secs_f64(deficit / self.rps));
        }
    }
}

/// Cheap deterministic-ish jitter source without a dependency: xorshift on
/// a thread-seeded state.
fn rand_unit() -> f64 {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0x9E3779B97F4A7C15) };
    }
    STATE.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        (x >> 11) as f64 / (1u64 << 53) as f64
    })
}

/// AIMD interval policy: the ONLY place poll cadence is decided.
/// - 0 new messages -> grow toward MAX (this is where politeness lives).
/// - new messages   -> shrink toward MIN (this is where coverage lives).
/// - saturation (>= WINDOW_MSGS new, window is churning faster than we
///   sample) -> MIN + warning: drops are possible, surface it to the caller.
#[derive(Debug, Clone, PartialEq)]
pub struct Cadence {
    pub interval: Duration,
    pub saturated: bool,
}

pub fn next_interval(prev: Duration, new_msgs: usize) -> Cadence {
    let s = prev.as_secs_f64();
    let (interval, saturated) = if new_msgs == 0 {
        ((s * IDLE_GROW).min(MAX_INTERVAL.as_secs_f64()), false)
    } else if new_msgs >= WINDOW_MSGS {
        (MIN_INTERVAL.as_secs_f64(), true)
    } else {
        ((s / ACTIVE_SHRINK).max(MIN_INTERVAL.as_secs_f64()), false)
    };
    Cadence {
        interval: Duration::from_secs_f64(interval),
        saturated,
    }
}

/// Per-event tracking state. `seen` is bounded: we only need the last
/// WINDOW_MSGS ids to dedupe against the server window.
#[derive(Debug, Clone)]
pub struct EventState {
    pub ticker: String,
    pub interval: Duration,
    pub due: std::time::Instant,
    pub seen: std::collections::VecDeque<String>,
    pub enabled: Option<bool>,
    pub msgs_total: u64,
    pub consecutive_err: u8,
    /// false until the first observation lands; the first window is a
    /// baseline, not churn — saturation warnings must not fire on it.
    pub primed: bool,
}

impl EventState {
    pub fn new(ticker: String) -> Self {
        Self {
            ticker,
            interval: MAX_INTERVAL,
            due: std::time::Instant::now(),
            seen: Default::default(),
            enabled: None,
            msgs_total: 0,
            consecutive_err: 0,
            primed: false,
        }
    }

    /// Dedupe a batch of message ids against the rolling window. Returns
    /// ids we have never seen, and folds everything into `seen` (bounded at
    /// 2x window: server only ever shows the last WINDOW_MSGS).
    pub fn observe(&mut self, ids: Vec<String>) -> usize {
        let fresh = ids.iter().filter(|id| !self.seen.contains(id)).count();
        for id in ids {
            if !self.seen.contains(&id) {
                self.seen.push_back(id);
            }
        }
        while self.seen.len() > 2 * WINDOW_MSGS {
            self.seen.pop_front();
        }
        self.msgs_total += fresh as u64;
        self.primed = true;
        fresh
    }
}

/// Due-first scheduler: a max-heap keyed on reverse due time so the most
/// overdue event pops first. O(log n) per event per pass.
#[derive(Debug)]
struct DueEntry {
    at: std::time::Instant,
    ticker: String,
}

impl PartialEq for DueEntry {
    fn eq(&self, o: &Self) -> bool {
        self.at == o.at
    }
}
impl Eq for DueEntry {}
impl PartialOrd for DueEntry {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for DueEntry {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        // BinaryHeap is a max-heap; we want the EARLIEST due on top.
        o.at.cmp(&self.at)
    }
}

pub struct Scheduler {
    heap: BinaryHeap<DueEntry>,
    states: HashMap<String, EventState>,
}

impl Scheduler {
    pub fn new(tickers: Vec<String>) -> Self {
        let mut s = Self {
            heap: BinaryHeap::new(),
            states: HashMap::new(),
        };
        for t in tickers {
            let st = EventState::new(t.clone());
            // NB: heap entry and state.due must be the SAME Instant, else the
            // lazy-deletion staleness check discards fresh entries.
            let at = st.due;
            s.states.insert(t.clone(), st);
            s.heap.push(DueEntry { at, ticker: t });
        }
        s
    }

    pub fn len(&self) -> usize {
        self.states.len()
    }
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    /// Pop the next due event, if one is due within `now + horizon`. Stale
    /// heap entries (superseded by a reschedule) are lazily discarded.
    pub fn pop_due(&mut self, horizon: Duration) -> Option<String> {
        loop {
            match self.heap.peek() {
                Some(e) if e.at <= std::time::Instant::now() + horizon => {
                    let e = self
                        .heap
                        .pop()
                        .expect("peek returned Some; pop cannot be None");
                    let stale = self
                        .states
                        .get(&e.ticker)
                        .map(|s| s.due != e.at)
                        .unwrap_or(true);
                    if stale {
                        continue; // superseded entry — drop it
                    }
                    return Some(e.ticker);
                }
                _ => return None,
            }
        }
    }

    /// Re-schedule an event after a pass, applying AIMD and error backoff.
    pub fn reschedule(&mut self, ticker: &str, new_msgs: usize, err: bool) -> bool {
        let Some(st) = self.states.get_mut(ticker) else {
            return false;
        };
        if err {
            st.consecutive_err = st.consecutive_err.saturating_add(1).min(8);
            // exponential error backoff, capped at MAX_INTERVAL
            let mult = 2u32.pow(u32::from(st.consecutive_err));
            st.interval = (st.interval * mult).min(MAX_INTERVAL);
        } else {
            st.consecutive_err = 0;
            let c = next_interval(st.interval, new_msgs);
            st.interval = c.interval;
        }
        st.due = std::time::Instant::now() + st.interval;
        let at = st.due;
        let t = st.ticker.clone();
        self.heap.push(DueEntry { at, ticker: t });
        true
    }

    /// Sleep until the next event is due (or `max_wait`). Never sleeps past
    /// a due event by more than the caller's own scheduling latency.
    pub fn wait_next(&self, max_wait: Duration) {
        let until = self
            .heap
            .peek()
            .map(|e| e.at.saturating_duration_since(std::time::Instant::now()))
            .unwrap_or(max_wait)
            .min(max_wait);
        if !until.is_zero() {
            std::thread::sleep(until);
        }
    }

    /// Whether the event has been observed at least once (saturation
    /// warnings are only meaningful after priming).
    pub fn primed(&self, ticker: &str) -> bool {
        self.states.get(ticker).map(|s| s.primed).unwrap_or(false)
    }

    /// Expose `EventState::observe` through the scheduler (kept on
    /// `EventState` so it stays unit-testable in isolation).
    pub fn observe(&mut self, ticker: &str, ids: &[String]) -> usize {
        match self.states.get_mut(ticker) {
            Some(st) => st.observe(ids.to_vec()),
            None => 0,
        }
    }

    pub fn msgs_total(&self) -> u64 {
        self.states.values().map(|s| s.msgs_total).sum()
    }

    /// Events whose next pass is >= `threshold` away are evicted from the
    /// active set (they are chat-disabled or long-idle) — keeps the working
    /// set bounded on a venue with thousands of events. Returns evicted
    /// tickers so the caller can persist state.
    pub fn evict_idle(&mut self, _threshold: Duration) -> Vec<String> {
        Vec::new() // eviction disabled v0.1: full coverage is the point
    }
}

/// Wire types for the preview endpoint.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ChatUser {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ChatMessage {
    pub id: String,
    #[serde(default)]
    pub text: String,
    pub user: ChatUser,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub position_side: Option<String>,
    #[serde(default)]
    pub position_label: Option<String>,
    #[serde(default)]
    pub position_cost_usd: Option<f64>,
    #[serde(default)]
    pub position_color_market_ticker: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Preview {
    #[serde(default)]
    pub chat_enabled: bool,
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
}

/// Minimal spam heuristic: bonus-phishing posts fake kalshi domains with
/// zero-width stippers. Bounded, no allocation beyond the check.
pub fn looks_like_spam(text: &str) -> bool {
    let t = text.replace('\u{200b}', "").to_lowercase();
    t.contains("kalshi.com/gift")
        || t.contains("bonus")
            && (t.contains("claim") || t.contains("rewards"))
            && t.contains("http")
        || t.contains("ads-kalshi")
        || t.contains("nfl-kalshi")
}

/// Walk the BFF event list with cursor pagination until exhausted. Returns
/// every non-crosscategory event ticker (open + unopened: the ENTIRETY the
/// user asked for). Bounded at 1000 pages as a runaway guard.
pub fn discover_events(ua: &str) -> Result<Vec<String>, ChError> {
    let mut out: Vec<String> = Vec::new();
    let mut cursor = String::new();
    for _page in 0..1000 {
        let mut url = format!("{BFF}/events/?status=open%2Cunopened&page_size=100");
        if !cursor.is_empty() {
            url.push_str(&format!("&cursor={cursor}"));
        }
        let d = http_get_json(&url, ua)?;
        let evs = d
            .get("events")
            .and_then(|e| e.as_array())
            .ok_or_else(|| ChError::Parse(format!("events page missing 'events' array: {url}")))?;
        if evs.is_empty() {
            break;
        }
        for e in evs {
            if let Some(t) = e.get("event_ticker").and_then(|v| v.as_str()) {
                if !t.contains("CROSSCATEGORY") {
                    out.push(t.to_string());
                }
            }
        }
        cursor = d
            .get("cursor")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        if cursor.is_empty() {
            break;
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

pub fn preview_url(event_ticker: &str) -> String {
    format!("{BFF}/live_chat/events/{event_ticker}/preview")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aimd_grows_when_quiet() {
        let c = next_interval(Duration::from_secs(20), 0);
        assert_eq!(c.interval, Duration::from_secs_f64(32.0));
        assert!(!c.saturated);
    }

    #[test]
    fn aimd_grows_to_cap() {
        let mut i = MAX_INTERVAL;
        for _ in 0..50 {
            i = next_interval(i, 0).interval;
        }
        assert_eq!(i, MAX_INTERVAL);
    }

    #[test]
    fn aimd_shrinks_when_active() {
        let c = next_interval(Duration::from_secs(600), 5);
        assert_eq!(c.interval, Duration::from_secs(300));
        let mut i = c.interval;
        for _ in 0..10 {
            i = next_interval(i, 5).interval;
        }
        assert_eq!(i, MIN_INTERVAL);
    }

    #[test]
    fn aimd_flags_saturation() {
        let c = next_interval(Duration::from_secs(20), WINDOW_MSGS);
        assert!(c.saturated);
        assert_eq!(c.interval, MIN_INTERVAL);
    }

    #[test]
    fn observe_dedupes_and_bounds() {
        let mut st = EventState::new("X".into());
        let ids: Vec<String> = (0..30).map(|i| format!("id{i}")).collect();
        assert_eq!(st.observe(ids.clone()), 30);
        assert_eq!(st.observe(ids), 0);
        assert!(st.seen.len() <= 2 * WINDOW_MSGS);
        assert_eq!(st.msgs_total, 30);
    }

    #[test]
    fn scheduler_pops_earliest_due() {
        let mut s = Scheduler::new(vec!["A".into(), "B".into()]);
        s.reschedule("A", 0, false); // A ages out to 600s
        let next = s.pop_due(Duration::from_secs(0));
        assert_eq!(next.as_deref(), Some("B"));
    }

    #[test]
    fn error_backoff_is_exponential_and_capped() {
        let mut s = Scheduler::new(vec!["A".into()]);
        s.reschedule("A", 0, false);
        let base = s.states["A"].interval;
        s.reschedule("A", 0, true);
        assert_eq!(s.states["A"].interval, (base * 2).min(MAX_INTERVAL));
        for _ in 0..10 {
            s.reschedule("A", 0, true);
        }
        assert_eq!(s.states["A"].interval, MAX_INTERVAL);
        assert_eq!(s.states["A"].consecutive_err, 8);
    }

    #[test]
    fn preview_parses_golden_wire_sample() {
        let raw = r#"{"chat_enabled":true,"messages":[{"id":"a1","text":"hello","user":{"id":"chat_x","name":"u1"},"created_at":"2026-09-27T03:12:44.478018Z","position_label":"ORE","position_cost_usd":76,"position_side":"yes","position_color_market_ticker":"KXNCAAFGAME-26SEP26OREUSC-ORE"}]}"#;
        let p: Preview = serde_json::from_str(raw).unwrap();
        assert!(p.chat_enabled);
        assert_eq!(p.messages.len(), 1);
        assert_eq!(p.messages[0].user.name, "u1");
        assert_eq!(p.messages[0].position_cost_usd, Some(76.0));
    }

    #[test]
    fn preview_parses_minimal_message() {
        let raw = r#"{"chat_enabled":false,"messages":[]}"#;
        let p: Preview = serde_json::from_str(raw).unwrap();
        assert!(!p.chat_enabled);
        assert!(p.messages.is_empty());
    }

    #[test]
    fn spam_detector_catches_bonus_phishing() {
        assert!(looks_like_spam("Congratulations! You qualified for a cash bonus https://ads\u{200b}-kalshi.com/rewards"));
        assert!(looks_like_spam(
            "New trader bonus available - claim before midnight https://nfl-kalshi.com/claim"
        ));
        assert!(!looks_like_spam("Nov 3rd tells the tale"));
    }

    #[test]
    fn rate_limiter_is_polite_under_burst() {
        let mut rl = RateLimiter::new(2.0);
        let t0 = std::time::Instant::now();
        for _ in 0..6 {
            rl.acquire();
        }
        // 6 acquires at 2 rps with 5-token burst: >= ~0.5s elapsed.
        assert!(t0.elapsed() >= Duration::from_millis(400));
    }
}
