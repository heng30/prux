//! 通用工具
//!
//! 不依赖 core/，供 core/ 与 cli/、modes/ 使用。

pub mod clipboard;
pub mod color;
pub mod cron;
pub mod display;
pub mod edit_diff;
pub mod file_lock;
pub mod find;
pub mod fuzzy;
pub mod glyphs;
pub mod http;
pub mod ignore;
pub mod image;
pub mod mime;
pub mod net;
pub mod output_files;
pub mod paths;
pub mod proxy;
pub mod terminal_caps;
pub mod terminal_colors;
pub mod terminal_image;
pub mod time;
pub mod tokens;
pub mod truncate;
pub mod wheel;
pub mod zip;
