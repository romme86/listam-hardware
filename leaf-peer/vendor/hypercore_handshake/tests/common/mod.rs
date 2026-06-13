pub mod js;

pub type Result<T> = core::result::Result<T, Box<dyn std::error::Error>>;

pub static LOOPBACK: &str = "127.0.0.1";
