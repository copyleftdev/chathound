# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] — 2026-09-26

### Added
- `scan`: full-venue event discovery (cursor-paginated) with per-event
  `chat_enabled` probing.
- `listen`: adaptive NDJSON tap with AIMD per-event intervals, global
  token-bucket rate limiting with jitter, 429/5xx global cooldown,
  exponential per-event error backoff, and saturation warnings.
- `events`, `stats` utilities.
- Position-annotated output records (side/label/cost/market) and a
  bonus-phishing spam flag.
- 11 unit tests covering the AIMD policy, scheduler lazy-deletion, rate
  limiter, wire parsing, and spam detection.
