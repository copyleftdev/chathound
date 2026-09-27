# chathound

[![CI](https://github.com/copyleftdev/chathound/actions/workflows/ci.yml/badge.svg)](https://github.com/copyleftdev/chathound/actions/workflows/ci.yml)
[![publish crates.io](https://github.com/copyleftdev/chathound/actions/workflows/publish.yml/badge.svg)](https://github.com/copyleftdev/chathound/actions/workflows/publish.yml)
[![crates.io](https://img.shields.io/crates/v/chathound.svg)](https://crates.io/crates/chathound)
[![crates.io downloads](https://img.shields.io/crates/d/chathound.svg)](https://crates.io/crates/chathound)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

**A polite, exhaustive tap of Kalshi live-market chat.** Listens to *every*
chat-enabled event on the exchange — sports slates, election markets,
weather, politics — and writes a clean NDJSON stream where every message is
annotated with the sender's **position** (side, size, and the market they're
holding), because sentiment with skin in the game is the only sentiment that
matters.

Companion to [tapehound](https://github.com/copyleftdev/tapehound) (the
Kalshi whale-fill scanner). tapehound hears the **money**; chathound hears
the **crowd**.

## Why

Kalshi exposes per-event live chat as an unauthenticated rolling window
(~21 most recent messages) with no push stream. "Hearing everything" is
therefore a polling problem, and naive polling is either rude to the
exchange or deaf to fast chats. chathound solves both with explicit,
tested algorithms:

- **AIMD adaptive polling** — per event, the poll interval ages from 20s up
  to 600s while quiet and snaps back toward 20s the moment new messages
  appear (multiplicative-increase / multiplicative-decrease, the same
  discipline TCP uses for congestion). Budget flows to where chatter is.
- **Global token bucket** — a hard ceiling on requests/second (default 1.5,
  burst 5) with ±20% jitter so a fleet of clients never syncs into a
  thundering herd.
- **Server-stress governor** — HTTP 429/5xx trips a 30s global cooldown;
  per-event failures back off exponentially (capped), so the exchange never
  sees a retry storm.
- **Saturation honesty** — when a chat churns its whole window between polls,
  chathound prints a `SATURATED` warning and tells you to raise the ceiling,
  instead of silently dropping messages.

## Install

```
cargo install --path .
# or from source
git clone https://github.com/copyleftdev/chathound && cargo build --release
```

## Usage

```
chathound scan    [OUT_DIR=data]        # walk the ENTIRE event venue, probe
                                        # chat_enabled per event -> chat_events_DATE.json
chathound events  [FILE]                # list chat-enabled events from a scan
chathound listen  [FILE] [OUT.ndjson] [MAX_RPS=1.5]
                                        # adaptive tap -> NDJSON stream (run forever)
chathound stats   [NDJSON]              # coverage + per-event volume report
```

Typical session:

```
chathound scan                       # one-time discovery (~1 req/event, polite)
chathound listen                     # leave running; tail the NDJSON
chathound stats data/chat_stream.ndjson
```

### NDJSON record

```json
{"ts_utc":1790480393,"event_ticker":"CONTROLH-2026","id":"…","text":"…",
 "user_id":"chat_…","user_name":"godwink777","created_at":"2026-09-27T02:02:01Z",
 "position_side":"yes","position_label":"Republican Party","position_cost_usd":227,
 "position_market":"CONTROLH-2026-RP","spam":false}
```

`spam` flags the bonus-phishing posts that infest these chats (fake
`ads-kalshi.com` domains with zero-width stippers) so downstream NLP can skip
them.

## Shannon entropy: the information content of the crowd

Every NDJSON record carries three incremental information-theoretic measures
for its event (see `src/entropy.rs`):

| Field | Meaning | Read it as |
|---|---|---|
| `ent_word_bits` | Shannon entropy of the word distribution | high = diverse discussion; ~0 = repetition (spam, a rant) |
| `ent_user_norm` | normalized speaker entropy `H/log2(n)` | 1.0 = distributed crowd; ~0 = one person's monologue |
| `ent_side_bits` | entropy of position-side distribution | ~1 bit = balanced YES/NO crowd; ~0 = echo chamber |

All are O(1) per message, exact over the observed distribution, with a bounded
vocabulary (100k words/event) so hostile chats cannot grow memory. These are
the inputs for contrarian signals (side entropy collapsing while volume
spikes) and honest reporting (volume without entropy is just noise).

## Guarantees & bounds (TigerStyle)

- One in-flight request at a time; no parallel connections.
- Every buffer bounded: dedupe window 2× server window, HTTP reader capped at
  64MB, event pages capped at 1000.
- No panics on the hot path; errors are explicit and back off.
- Deterministic outputs; atomic scan-file writes (tmp + rename).

## What it deliberately does not do

- No login, no posting, no reactions — read-only, and only the public
  preview surface.
- No undisclosed endpoints: everything chathound touches is the same
  unauthenticated route the Kalshi web app itself uses.

## License

MIT — see [LICENSE](LICENSE).
