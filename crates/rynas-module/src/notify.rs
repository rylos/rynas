//! Notifications forwarded by a module's own tools.
//!
//! rynasd opens its local forward socket (`/run/rynas/notify.sock`, root only, `POST
//! /v1/notify`) only when a module provides an [`EventForward`]; the module turns the body its
//! tools send into a [`NewEvent`]. The events carry a module code (`<module>.<event>`) whose
//! texts live with the module's web interface dictionary.

use rynas_notify::NewEvent;

pub trait EventForward: Send + Sync {
    /// Largest body accepted.
    fn max_body(&self) -> usize;

    /// The event of one forwarded body and whether it skips the repeat suppression.
    fn parse(&self, content_type: Option<&str>, body: &[u8]) -> Result<(NewEvent, bool), String>;
}
