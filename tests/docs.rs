//! Keeps the documentation and the configuration schema in step: the example file and every
//! complete example in docs/CONFIGURATION.md must load and validate, and every key must be
//! documented.

use coinbase_sweeper::config::{Command, Config};

fn read(path: &str) -> String {
    std::fs::read_to_string(format!("{}/{path}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn validate(text: &str, origin: &str) {
    let cfg = Config::parse(text).unwrap_or_else(|e| panic!("{origin}: {e:#}"));
    if cfg.sweep.is_some() {
        cfg.validate_for(Command::Sweep)
            .unwrap_or_else(|e| panic!("{origin} [sweep]: {e:#}"));
    }
    if cfg.refill.is_some() {
        cfg.validate_for(Command::Refill)
            .unwrap_or_else(|e| panic!("{origin} [refill]: {e:#}"));
    }
}

#[test]
fn example_config_is_valid() {
    validate(&read("sweeper.example.toml"), "sweeper.example.toml");
}

#[test]
fn configuration_examples_are_valid() {
    let doc = read("docs/CONFIGURATION.md");
    let blocks: Vec<&str> = doc
        .split("```toml\n")
        .skip(1)
        .map(|b| b.split("```").next().unwrap())
        .collect();
    assert!(blocks.len() >= 2, "one complete example per setup");
    for (i, b) in blocks.iter().enumerate() {
        validate(b, &format!("CONFIGURATION.md example {}", i + 1));
    }
}

#[test]
fn every_key_is_documented_and_in_the_example() {
    let doc = read("docs/CONFIGURATION.md");
    let example = read("sweeper.example.toml");
    let keys = [
        "shielded_address",
        "url",
        "user",
        "cookie_file",
        "timeout_secs",
        "submit_timeout_secs",
        "read_retries",
        "state_dir",
        "interval_secs",
        "max_tip_lag_blocks",
        "operation_timeout_secs",
        "confirmation_timeout_secs",
        "skipped_cycles_before_error",
        "max_backoff_secs",
        "rejection_pause_secs",
        "coinbase_addresses",
        "min_inputs",
        "max_wait_blocks",
        "max_inputs",
        "max_batches_per_cycle",
        "alert_unswept_zec",
        "mode",
        "destination",
        "destinations",
        "external_destination",
        "external_destinations",
        "low_water",
        "target",
        "reserve",
        "minconf",
        "fee_headroom",
        "min_transfer",
        "max_per_transfer",
        "max_per_day",
        "wait_confirmations",
    ];
    for k in keys {
        assert!(
            doc.contains(&format!("| `{k}` |")),
            "{k} missing from CONFIGURATION.md"
        );
    }
    // Keys that only apply to the other refill mode, or are alternatives, may be commented out.
    for k in keys {
        assert!(example.contains(k), "{k} missing from sweeper.example.toml");
    }
    // Every key the schema accepts is in the list above: an unknown key is rejected, so a key
    // added to the schema but not here fails `example_config_is_valid` only if used. Guard by
    // checking the schema rejects a misspelt key.
    let bad = read("sweeper.example.toml").replace("min_inputs", "min_input");
    assert!(Config::parse(&bad).is_err());
}
