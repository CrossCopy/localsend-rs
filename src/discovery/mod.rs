pub mod http;
pub mod multicast;
pub mod traits;

pub use http::HttpDiscovery;
pub use multicast::MulticastDiscovery;
pub use traits::Discovery;
