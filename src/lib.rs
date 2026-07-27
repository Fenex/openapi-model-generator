pub mod cli;
pub mod document;
pub mod error;
pub mod generator;
pub mod models;
pub mod parser;

pub use document::{load_openapi_from_json, load_openapi_from_path, load_openapi_from_yaml};
pub use error::Error;
pub use generator::{generate_models, GenerateMode};
pub use parser::parse_openapi;

pub type Result<T> = std::result::Result<T, Error>;
