//! Transfer jobs: [`manager`] (in rynasd) keeps them, `rynas-files-worker job` (as the user) runs them,
//! [`protocol`] is what they say to each other.

pub mod manager;
pub mod protocol;
