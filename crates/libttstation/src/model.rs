use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServingStatus {
    Idle,
    Serving(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoxRecord {
    pub name: String,
    pub host: String,
    pub ctrl_port: u16,
    pub chips: String,
    pub status: ServingStatus,
    pub apiver: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    pub base_url: String,
    pub model: String,
    pub requires_key: bool,
}

impl ServingStatus {
    pub fn to_txt(&self) -> String {
        match self {
            ServingStatus::Idle => "idle".to_string(),
            ServingStatus::Serving(model) => format!("serving:{}", model),
        }
    }

    pub fn from_txt(s: &str) -> anyhow::Result<Self> {
        if s == "idle" {
            Ok(ServingStatus::Idle)
        } else if let Some(model) = s.strip_prefix("serving:") {
            Ok(ServingStatus::Serving(model.to_string()))
        } else {
            Err(anyhow::anyhow!("Invalid ServingStatus format: {}", s))
        }
    }
}

pub fn txt_encode(rec: &BoxRecord) -> Vec<(String, String)> {
    vec![
        ("name".to_string(), rec.name.clone()),
        ("apiver".to_string(), rec.apiver.to_string()),
        ("chips".to_string(), rec.chips.clone()),
        ("status".to_string(), rec.status.to_txt()),
        ("ctrl".to_string(), rec.ctrl_port.to_string()),
    ]
}

pub fn txt_decode(
    _name: &str,
    host: &str,
    _port: u16,
    txt: &HashMap<String, String>,
) -> anyhow::Result<BoxRecord> {
    let name_val = txt
        .get("name")
        .ok_or_else(|| anyhow::anyhow!("Missing required key: name"))?
        .clone();

    let chips = txt
        .get("chips")
        .ok_or_else(|| anyhow::anyhow!("Missing required key: chips"))?
        .clone();

    let status_str = txt
        .get("status")
        .ok_or_else(|| anyhow::anyhow!("Missing required key: status"))?;
    let status = ServingStatus::from_txt(status_str)?;

    let ctrl_port = txt
        .get("ctrl")
        .ok_or_else(|| anyhow::anyhow!("Missing required key: ctrl"))?
        .parse::<u16>()?;

    let apiver: u8 = txt
        .get("apiver")
        .map(|s| s.parse::<u8>())
        .transpose()?
        .unwrap_or(1);

    Ok(BoxRecord {
        name: name_val,
        host: host.to_string(),
        ctrl_port,
        chips,
        status,
        apiver,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_roundtrips_through_txt_string() {
        assert_eq!(ServingStatus::Idle.to_txt(), "idle");
        assert_eq!(
            ServingStatus::Serving("llama3".into()).to_txt(),
            "serving:llama3"
        );
        assert_eq!(
            ServingStatus::from_txt("idle").unwrap(),
            ServingStatus::Idle
        );
        assert_eq!(
            ServingStatus::from_txt("serving:llama3").unwrap(),
            ServingStatus::Serving("llama3".into())
        );
    }

    #[test]
    fn txt_decode_builds_boxrecord() {
        let mut txt = std::collections::HashMap::new();
        txt.insert("name".into(), "qb2-lab".into());
        txt.insert("apiver".into(), "1".into());
        txt.insert("chips".into(), "4xBH".into());
        txt.insert("status".into(), "idle".into());
        txt.insert("ctrl".into(), "8765".into());
        let rec = txt_decode("qb2-lab", "qb2-lab.local", 8765, &txt).unwrap();
        assert_eq!(rec.name, "qb2-lab");
        assert_eq!(rec.chips, "4xBH");
        assert_eq!(rec.ctrl_port, 8765);
        assert_eq!(rec.status, ServingStatus::Idle);
    }
}
