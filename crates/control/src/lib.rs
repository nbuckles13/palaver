//! Reports and plan as tracks, flood rule, conductor election and hand-over.

mod node;

pub use node::{
    AUDIO_TOP_N, Config, FIRST_TERM, LocalView, Node, Output, PlanVerdict, REPORT_INTERVAL,
    REPORT_TIMEOUT, ReportVerdict,
};
