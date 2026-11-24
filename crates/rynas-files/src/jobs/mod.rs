//! Transfer jobs: [`manager`] (in rynasd) keeps them, `rynas-files-worker job` (as the user) runs them,
//! [`protocol`] is what they say to each other, [`notice`] the notification of a long one that ended.

pub mod manager;
pub mod notice;
pub mod protocol;
