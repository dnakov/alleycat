pub mod frame;
pub mod message;
pub mod ready;

pub const PROTOCOL_VERSION: u32 = 1;

pub use frame::{FrameError, read_frame_json, write_frame_json};
pub use message::{ConnectAck, ConnectHello, OpenRequest, OpenResponse, Target};
pub use ready::ReadyFile;
