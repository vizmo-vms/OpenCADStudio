//! The SecurePlan ribbon tab (DSK-06). Empty until task F4a fills it.

use crate::modules::{CadModule, RibbonGroup};

pub struct SecurePlanModule;

impl CadModule for SecurePlanModule {
    fn id(&self) -> &'static str {
        "secureplan"
    }

    fn title(&self) -> &'static str {
        "SecurePlan"
    }

    fn ribbon_groups(&self) -> &[RibbonGroup] {
        &[]
    }
}
