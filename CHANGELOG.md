# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Nothing yet.

## [0.1.0] - 2026-08-18

First release. An agent-ergonomic research CLI built on the [AXI](https://axi.md)
design principles: a tool an agent calls, returning TOON rather than prose.

### Added

**Core engine**

- `research`, `compare`, `report`, `source`, `watch`, `pack`, `cache` and `setup`
  commands. A bare `pilotage` prints live data, never help text.
- TOON serializer with key hoisting, definitive empty states (`key[0]: none`) and a
  `next:` suggestion on every command.
- Concurrent fetching (16 by default) with per-host rate limiting, retry with backoff,
  and robots.txt enforcement for markup sources.
- Three-tier SQLite cache: raw responses by TTL, extraction results by
  `(content_hash, extractor_version)` that never expire, and complete runs for 90 days.
- Three-rung extraction ladder — JSON field maps, CSS/regex selectors, then cheap-model
  extraction only for what the rungs above could not reach.
- Weighted scoring with `ok` / `warn` / `fail` / `unknown` signals, critical metrics that
  force a `risk` verdict, and `insufficient-data` below 40% coverage.
- Contradiction detection: sources that disagree beyond a pack's tolerance are reported,
  not reconciled.
- Self-contained HTML reports, single-run and side-by-side.
- `PILOTAGE_REPLAY=1` serves the cache and refuses the network, so a recorded run
  replays deterministically offline.
- Documented exit codes: `0` ok, `1` usage, `2` config, `3` no data, `4` budget.

**Packs**

- `topic` — any subject at all, from 11 free keyless sources: Wikipedia search and 24
  months of pageviews, OpenAlex, PubMed, Crossref, GitHub search, Stack Overflow, npm,
  and Hacker News stories and comments.
- `oss` — repository health and technical due diligence from the GitHub REST API.
- `crypto` — protocol health from DefiLlama and CoinGecko.
- `saas` — market, product and traction signals for a SaaS company.
- `aivideo` — AI video vendors: pricing, unit economics, public user feedback and
  operational incident history.
- `probe` — credential-gated latency probes, never auto-detected.

**Pack authoring**

- Source kinds `api`, `html`, `browser` (via `chrome-devtools-axi`) and `probe`.
- JSON paths supporting `.a.b`, `.a[0]`, `.a[].b` and `.a{}` (object keys as a list).
- Aggregations: `count`, `sum`, `avg`, `max`, `min`, `first`, `unique`, `days_since`,
  `pct_change` and `ratio`.
- `filter` narrows an array before aggregation; `take` windows it; `pin` keeps a value
  visible in the compact view; `over` supplies a divisor for `ratio`.
- Template variables including `{brand}`, date windows, `_enc` and `_us` spellings, and
  `{env.NAME}`.
- `pack validate` catches duplicate ids, unparseable thresholds, metrics reading facts no
  source produces, and ungated probes.
- User packs in `~/.pilotage/packs/` load at runtime and override built-ins of the same
  name. No recompile.

**Distribution**

- `install.sh` for `curl | sh` installation, with SHA-256 verification.
- GitHub Actions CI: fmt, clippy, tests on Linux/macOS/Windows, MSRV check, pack
  validation and installer linting.
- Release workflow building native binaries for x86_64 and aarch64 Linux and macOS, and
  x86_64 Windows.

### Security

- Probe credential gates are enforced before the request is built, so a probe without its
  key never submits a billable job.
- API keys are redacted from error output; providers echo them back in 401 bodies, and
  `degraded` entries end up in shared reports.
- All HTML report interpolation is escaped, including scraped page titles and source URLs.

### Known limitations

- `topic` measures a string, not a concept. Ambiguous queries need disambiguation —
  `topic:Go` matches the board game. Every run pins `matched_article` so this is visible.
- Brand-name search on Hacker News is noisy for names that tokenise to common words.
- `compare` pivots on metric id, so comparing targets across different packs produces a
  table of dashes.
- The `probe` source kind is tested end to end against a local server but has not been
  run against a live vendor API.

[Unreleased]: https://github.com/lambiengcode/pilotage/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/lambiengcode/pilotage/releases/tag/v0.1.0
