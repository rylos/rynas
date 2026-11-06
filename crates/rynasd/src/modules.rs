//! Detection of the hardware modules (D-MODULE, `rynas_module`): one line per module, used by
//! rynasd at start and by `rynasctl` for the commands that run without rynasd. A plain Debian
//! has none, and every extension point is then empty.

use std::path::Path;
use std::sync::Arc;

use rynas_module::{Module, Modules};

/// The modules of the machine below `hw_root` (`/sys`, `/proc`, `/run`) whose system layer's
/// files are below the configuration root `root`.
pub fn detect(hw_root: &Path, root: &Path) -> Modules {
    let mut list: Vec<Arc<dyn Module>> = Vec::new();
    if let Some(m) = rynas_hw_synology::module::SynologyModule::detect(hw_root, root) {
        list.push(Arc::new(m));
    }
    Modules::new(list)
}
