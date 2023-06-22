//! A parser for the command line flags and configuration file.
use crate::config::Config;
use clap::Parser;
use http::Uri;
use std::{fs::File, io, path::PathBuf};
use thiserror::Error;

#[allow(missing_docs)]
#[derive(Debug, Error)]
pub enum CliError {
    #[error("{0}")]
    Io(io::Error),
    #[error("An error occurred while deserialized the provided configuration: {0}")]
    Deserialize(String),
    #[error("An error occurred while validating the provided configuration: {0}")]
    Validation(String),
}
/// This struct is use to provide a command line interface to the adapter.
#[derive(Parser)]
#[clap(version = "0.0.0", author = "BitBolt Team")]
pub struct Cli {
    /// This field contains the path to the config file.
    pub config: PathBuf,
}

impl Cli {
    /// Loads the config from the provided `config` argument.
    pub fn get_config(&self) -> Result<Config, CliError> {
        // The expected JSON config.
        let file = File::open(&self.config).map_err(CliError::Io)?;
        let config: Config =
            serde_json::from_reader(file).map_err(|err| CliError::Deserialize(err.to_string()))?;

        // Validate proxy URL.
        // Check for general validation errors.
        if let Some(socks_proxy) = &config.socks_proxy {
            let uri = socks_proxy
                .parse::<Uri>()
                .map_err(|_| CliError::Validation("Failed to parse socks_proxy url".to_string()))?;
            // scheme, host, port should be present. 'socks5://someproxy.com:80'
            if uri.scheme().is_none() || uri.host().is_none() || uri.port().is_none() {
                return Err(CliError::Validation(
                    "Make sure socks proxy url contains (scheme,host,port)".to_string(),
                ));
            }
        }
        Ok(config)
    }
}