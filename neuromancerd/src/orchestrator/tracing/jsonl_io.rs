use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use neuromancer_core::rpc::ThreadEvent;

use crate::orchestrator::error::System0Error;

pub(crate) fn append_jsonl_line<T: serde::Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), System0Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            System0Error::Internal(format!(
                "failed to create directory '{}': {err}",
                parent.display()
            ))
        })?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| {
            System0Error::Internal(format!(
                "failed to open jsonl file '{}': {err}",
                path.display()
            ))
        })?;
    serde_json::to_writer(&mut file, value)
        .map_err(|err| System0Error::Internal(format!("failed to encode jsonl event: {err}")))?;
    file.write_all(b"\n")
        .map_err(|err| System0Error::Internal(format!("failed to write jsonl newline: {err}")))?;
    file.flush()
        .map_err(|err| System0Error::Internal(format!("failed to flush jsonl file: {err}")))?;
    Ok(())
}

pub(crate) fn read_jsonl_lines(path: &Path) -> Result<Vec<String>, System0Error> {
    let file = fs::File::open(path).map_err(|err| {
        System0Error::Internal(format!(
            "failed to open jsonl file '{}': {err}",
            path.display()
        ))
    })?;
    let reader = BufReader::new(file);
    let mut lines = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|err| {
            System0Error::Internal(format!(
                "failed to read jsonl file '{}': {err}",
                path.display()
            ))
        })?;
        if !line.trim().is_empty() {
            lines.push(line);
        }
    }
    Ok(lines)
}

pub(crate) fn read_thread_events_from_path(path: &Path) -> Result<Vec<ThreadEvent>, System0Error> {
    let mut events = Vec::<ThreadEvent>::new();
    for line in read_jsonl_lines(path)? {
        match serde_json::from_str::<ThreadEvent>(&line) {
            Ok(event) => events.push(event),
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "skipping_malformed_thread_event"
                );
            }
        }
    }
    events.sort_by(|a, b| {
        if a.seq == b.seq {
            a.ts.cmp(&b.ts)
        } else {
            a.seq.cmp(&b.seq)
        }
    });
    Ok(events)
}
