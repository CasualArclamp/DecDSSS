// made by CasualArclamp
//! The link layer: what rides in the frames. A file is wrapped in a small
//! container and cut into data shards; the sender adds coded shards (Cauchy
//! Reed–Solomon over GF(2^16)), so any `count` distinct frames rebuild the file,
//! however they are lost, and each further pass sends new coded frames. The
//! receiver gathers frames from any number of segments and passes until the
//! file is whole.

pub mod assemble;
pub mod container;
pub mod erasure;
pub mod send;

pub use assemble::{Assembler, Progress, ReceivedFile};
pub use send::{Plan, SendSpec, plan, segments};
