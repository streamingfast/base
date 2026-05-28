#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod error;
pub use error::Error;

mod tracer;
pub use tracer::{FLASHBLOCK_TRACER_ID, FlashblocksTracerHandle};

mod processor;
pub use processor::{ClockFn, FirehoseFlashblocksProcessor};

mod streamer;
pub use streamer::FirehoseFlashblocksStreamer;
