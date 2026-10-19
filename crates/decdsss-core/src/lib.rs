// made by CasualArclamp
//! The air interface's fixed numbers: bandwidth profiles, the mode ladder and
//! the rates they give. Everything here is plain arithmetic on the constants
//! in `docs/DESIGN.md`, so the CLI, the modem and the GUI agree on them.

pub mod mode;
pub mod profile;

pub use mode::{CodeRate, Family, Mode};
pub use profile::Profile;
