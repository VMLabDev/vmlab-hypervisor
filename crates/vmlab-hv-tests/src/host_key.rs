//! Runtime-only loading of the owner's SMC key. Diagnostics never include it.

use std::path::Path;
use wcl_lang::{Document, Environment, Registry, Value, disk_loader};

pub fn parse_host_osk(source: &str) -> Result<Option<String>, &'static str> {
    let mut registry = Registry::new();
    // Only evaluate the requested field. The owner's vmlab validates the full
    // host schema; keeping a second copy here would make the schemas diverge.
    registry.register("vmlab-host.wcl", "");
    let doc = Document::open_at_with_loader(
        source,
        "host-config",
        None,
        &Environment::new(),
        registry.loader(disk_loader()),
    )
    .map_err(|_| "cannot parse host configuration")?;
    let Some(host) = doc.blocks().find(|block| block.kind() == "host") else {
        return Ok(None);
    };
    let Some(field) = host.fields().find(|field| field.name() == "apple_smc_osk") else {
        return Ok(None);
    };
    match field
        .value()
        .map_err(|_| "cannot evaluate host.apple_smc_osk")?
    {
        Value::Utf8(value) | Value::Ascii(value) => checked(value.to_string()).map(Some),
        _ => Err("host.apple_smc_osk must be a string"),
    }
}

fn checked(value: String) -> Result<String, &'static str> {
    if value.len() == 64 && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        Ok(value)
    } else {
        Err("SMC key must be exactly 64 printable ASCII characters")
    }
}

pub fn load_host_osk() -> Result<Option<String>, &'static str> {
    if let Ok(value) = std::env::var("VMLAB_HV_TEST_APPLE_SMC_OSK") {
        return checked(value).map(Some);
    }
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(None);
    };
    let directory = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new(&home).join(".config"));
    let path = directory.join("vmlab/config.wcl");
    if !path.exists() {
        return Ok(None);
    }
    let source = std::fs::read_to_string(path).map_err(|_| "cannot read host configuration")?;
    parse_host_osk(&source)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_expressions_without_exposing_invalid_values() {
        let value = "FAKE".repeat(16);
        let source = format!(
            "import <vmlab-host.wcl>\nlet secret = \"{value}\"\nhost {{ apple_smc_osk = secret }}"
        );
        assert_eq!(parse_host_osk(&source).unwrap(), Some(value));
        assert_eq!(parse_host_osk("host { }").unwrap(), None);
        assert!(
            parse_host_osk("host { apple_smc_osk = \"BAD-KEY\" }")
                .unwrap_err()
                .contains("64 printable")
        );
    }
}
