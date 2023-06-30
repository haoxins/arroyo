use serde::{Deserialize, Serialize};
use typify::import_types;

pub mod blackhole;
pub mod impulse;
pub mod kafka;
pub mod nexmark;
pub mod sse;

import_types!(schema = "../connector-schemas/common.json",);