//! Times the native library on every document in a directory.
//!
//! Prints one line of JSON per measurement, for `bench/run.js` to read.

use core::error::Error;
use core::hint::black_box;
use core::time::Duration;
use std::env;
use std::fs;
use std::io::{Write, stdout};
use std::time::Instant;

use serde_json::{Value, json};
use unbinder::{CircularPolicy, Options, dereference_schema_shared, dereference_schema_with};

/// The environment variable that names the directory of documents.
const FIXTURES: &str = "UNBINDER_BENCH_FIXTURES";
/// How long a measurement keeps sampling.
const BUDGET: Duration = Duration::from_secs(3);
const MINIMUM_SAMPLES: usize = 5;

fn main() -> Result<(), Box<dyn Error>> {
    let directory = env::var(FIXTURES).map_err(|cause| format!("{FIXTURES}: {cause}"))?;
    let mut paths = fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    for path in paths {
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let fixture = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or("a fixture name is not UTF-8")?;
        let text = fs::read_to_string(&path)?;
        let schema: Value = serde_json::from_str(&text)?;
        for (mode, circular) in [
            ("ignore", CircularPolicy::Ignore),
            ("expand", CircularPolicy::Expand),
        ] {
            // The comparison is of speed, so the size limit is out of the way.
            let options = Options {
                circular,
                max_bytes: usize::MAX,
                ..Options::default()
            };
            report(fixture, mode, "text to text", || {
                let parsed = serde_json::from_str(&text)?;
                let shared = dereference_schema_shared(&parsed, &options)?;
                Ok(serde_json::to_string(&shared)?)
            })?;
            report(fixture, mode, "value to value", || {
                Ok(dereference_schema_with(&schema, &options)?)
            })?;
            // What a caller pays who keeps the shared form, or writes it out
            // somewhere else later.
            report(fixture, mode, "value to shared", || {
                Ok(dereference_schema_shared(&schema, &options)?)
            })?;
        }
    }
    Ok(())
}

/// Samples `run` and prints how long each call took. The output of a call is
/// dropped after its clock has stopped.
fn report<Output>(
    fixture: &str,
    mode: &str,
    task: &str,
    mut run: impl FnMut() -> Result<Output, Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    let started = Instant::now();
    let mut nanoseconds = Vec::new();
    let mut failure = None;
    while nanoseconds.len() < MINIMUM_SAMPLES || started.elapsed() < BUDGET {
        let sample = Instant::now();
        let output = black_box(run());
        let elapsed = sample.elapsed();
        match output {
            Ok(_) => nanoseconds.push(u64::try_from(elapsed.as_nanos())?),
            Err(cause) => {
                failure = Some(cause.to_string());
                break;
            }
        }
    }
    writeln!(
        stdout(),
        "{}",
        json!({
            "fixture": fixture,
            "mode": mode,
            "task": task,
            "nanoseconds": nanoseconds,
            "error": failure,
        })
    )?;
    Ok(())
}
