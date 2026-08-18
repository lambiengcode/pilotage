//! Pilotage — an agent-ergonomic research CLI.
//!
//! Pilotage is a tool an agent calls, not an agent itself. It does the deterministic,
//! token-expensive work — fetch, parse, cache, aggregate, score — and returns a dense
//! TOON summary. Open-ended reasoning stays with the caller, which is already a
//! reasoning loop and already paid for.

pub mod cache;
pub mod cmd;
pub mod extract;
pub mod fact;
pub mod fetch;
pub mod pack;
pub mod report;
pub mod research;
pub mod score;
pub mod testserver;
pub mod toon;
pub mod util;

/// Exit codes. Distinct enough that a caller can branch on them without parsing text.
pub mod exit {
    /// Success, including a run that legitimately found nothing.
    pub const OK: i32 = 0;
    /// Bad invocation: unknown target, unparseable arguments, no matching pack.
    pub const USAGE: i32 = 1;
    /// Required configuration or credential is missing.
    pub const CONFIG: i32 = 2;
    /// Every source failed. A run where only some failed stays OK and reports
    /// `degraded`.
    pub const NO_DATA: i32 = 3;
    /// The cost ceiling was reached before the run could finish.
    pub const BUDGET: i32 = 4;
}
