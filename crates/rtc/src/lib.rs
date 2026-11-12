//! WebRTC plumbing shared by the mpvlink plugin and the test sender.

pub mod h264;
pub mod peer;
pub mod publish;
pub mod signaling;

pub use mpvlink_proto as proto;
// Re-exported so users don't need direct dependencies with matching versions.
pub use rtc;
pub use webrtc;
