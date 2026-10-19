// made by CasualArclamp
//! The link layer: what rides in the frames. A file is wrapped in a small
//! container, cut into frames, grouped into blocks and segments by the
//! sender; the receiver gathers frames from any number of segments and
//! passes (and passes again: a repeated transmission fills the gaps) until
//! the file is whole.

pub mod assemble;
pub mod container;
pub mod send;

pub use assemble::{Assembler, Progress, ReceivedFile};
pub use send::{SendSpec, segments};
