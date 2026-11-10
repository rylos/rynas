//! The rynas curated app catalogue (D-APPCAT, `docs/phase2/docker-apps.md` § 6).
//!
//! Pure data in, files out: no Docker access. The flow:
//! 1. [`Catalog::load`] reads `apps/<id>/` entries and validates everything (manifest, form,
//!    compose lint for every overlay, migrations, test values, metadata);
//! 2. the wizard shows [`form::FormSchema`], checks [`plan::port_conflicts`] and
//!    [`plan::ram_gate`];
//! 3. [`render::render`] turns the answers and the rynas-provided values into an ordinary
//!    stack folder ([`render::RenderedStack`]: `compose.yaml` + `.env` + folders to create),
//!    which the Docker engine crate writes to the stack subvolume and brings up;
//! 4. updates: [`plan::update_plan`] compares the installed `x-rynas` version and images with
//!    the catalogue, [`plan::prepare_update`] migrates the stored values and re-renders;
//! 5. [`render::convert_to_custom`] drops `x-rynas`: the stack stays, rynas forgets the app.

pub mod catalog;
pub mod compose;
pub mod error;
pub mod form;
pub mod manifest;
#[cfg(feature = "online")]
pub mod online;
pub mod plan;
pub mod render;
pub mod version;

pub use catalog::{Catalog, CatalogApp};
pub use error::{Issue, Issues, RenderError};
pub use manifest::AppManifest;
pub use render::{InstallRequest, Provided, RenderedStack, StackFile};
pub use version::Version;
