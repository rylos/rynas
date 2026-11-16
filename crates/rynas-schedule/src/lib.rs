//! rynas schedules, without I/O (also built for the web interface, without the `next`
//! feature):
//!
//! - [`spec`]: systemd calendar expressions (`OnCalendar=`) parsed and normalized as systemd
//!   257 does;
//! - [`next`] (feature `next`): when an expression elapses, with systemd's DST behaviour;
//! - [`recurrence`]: the preset schedules of the editor, to and from `OnCalendar=`;
//! - [`describe`]: a schedule as structured words the interface translates;
//! - [`cal`]: the Task Scheduler's calendar builder (moved from the web interface).
//!
//! The DST rules systemd applies (checked on Debian trixie's systemd 257, golden files in
//! `tests/golden/`): a local time that does not exist on the spring-forward day is skipped
//! that day, a local time that exists twice on the fall-back day happens once (the first
//! time). See [`next`] for the details.

pub mod cal;
pub mod describe;
#[cfg(feature = "next")]
pub mod next;
pub mod recurrence;
pub mod spec;

pub use describe::{Description, describe};
#[cfg(feature = "next")]
pub use next::next_runs;
pub use recurrence::{MonthDay, Nth, Parsed, Recurrence, TimeOfDay, Weekdays, classify};
pub use spec::{CalendarSpec, Error, normalize};
