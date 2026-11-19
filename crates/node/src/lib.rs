//! One node's core: the khef and the control logic, driven together.
//!
//! [`palaver_khef_core::Khef`] moves bytes between neighbours and knows nothing of plans;
//! [`palaver_control::Node`] decides what the plan is and knows nothing of connections.
//! Neither depends on the other. This crate is the node that owns both, and holds the rules
//! that join them:
//!
//! - **Control objects go up, then down.** What the khef receives is given to the control
//!   logic to check; what the control logic accepts or makes is given to the khef to flood.
//! - **The plan is carried out.** When a plan is adopted, the node dials the planned peers
//!   it has no link to (the lower participant id dials), tells the khef where audio goes,
//!   and closes links the plan does not name, but only once both ends are wired as the plan
//!   says (which it reads from the other end's report). Until then such a link may be the
//!   other end's only way to receive the plan.
//! - **Links are reported.** A link that comes up or dies, and a dial that fails, change
//!   what the control logic reports.
//! - **A node that is alone looks for the meeting.** A joiner dials the entry points it was
//!   given, one after another. A member left with no link at all dials the members of the
//!   last plan it saw. Nobody else can do this for it: the others cannot tell a node that
//!   was cut off from one that left.
//! - **A joiner speaks once it is connected.** Its control logic starts when its first link
//!   is up, so its first report already names that link.
//!
//! Sans-IO: the caller owns the connections and the clock.

mod node;

pub use node::{Config, Counters, Node, Output};
