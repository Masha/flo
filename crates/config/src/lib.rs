use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

pub mod error;

use error::*;

#[derive(Debug, Serialize, Deserialize)]
pub struct ClientConfig {
  pub local_port: u16,
  pub user_data_path: Option<PathBuf>,
  pub installation_path: Option<PathBuf>,
  pub lobby_domain: String,
}

impl Default for ClientConfig {
  fn default() -> Self {
    ClientConfig {
      local_port: flo_constants::CONNECT_WS_PORT,
      user_data_path: None,
      installation_path: None,
      lobby_domain: flo_constants::LOBBY_DOMAIN.to_string(),
    }
  }
}

impl ClientConfig {
  pub fn from_env() -> Result<Self> {
    let mut config = ClientConfig::default();

    config.apply_env();

    Ok(config)
  }

  pub fn load() -> Result<Self> {
    let mut config: Self = toml::from_str(&fs::read_to_string("flo.toml")?)?;

    config.apply_env();

    Ok(config)
  }

  pub fn save(&self) -> Result<()> {
    fs::write("flo.toml", toml::to_string_pretty(self)?).map_err(Into::into)
  }

  fn apply_env(&mut self) {
    use std::env;

    if let Ok(Some(port)) = env::var("FLO_LOCAL_PORT")
      .ok()
      .map(|v| v.parse())
      .transpose()
    {
      self.local_port = port;
    }

    if let Some(path) = env::var("FLO_USER_DATA_PATH").ok().map(PathBuf::from) {
      self.user_data_path = Some(path);
    }

    if let Some(path) = env::var("FLO_INSTALLATION_PATH").ok().map(PathBuf::from) {
      self.installation_path = Some(path);
    }

    if let Some(domain) = env::var("FLO_LOBBY_DOMAIN").ok() {
      self.lobby_domain = domain;
    }
  }
}

#[test]
fn test_client() {
  let config: ClientConfig = toml::from_str(
    r#"user_data_path = "C:\\Users\\fluxx\\OneDrive\\Documents\\Warcraft III"
installation_path = "C:\\Program Files (x86)\\Warcraft III""#,
  )
  .unwrap();
  dbg!(&config);

  dbg!(toml::to_string_pretty(&config).unwrap());
}