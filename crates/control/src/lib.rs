//! Reports and plan as tracks, flood rule, conductor election and hand-over.

mod node;

pub use node::{
    AUDIO_TOP_N, Config, DROP_HOLD, FIRST_TERM, LocalView, Node, Output, PlanVerdict,
    REPORT_MIN_GAP, REPORT_REFRESH, REPORT_TIMEOUT, ReportVerdict,
};
