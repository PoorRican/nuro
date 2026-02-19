use std::collections::HashMap;
use std::path::Path;

use crate::SkillError;

pub struct ParsedCsv {
    pub headers: Vec<String>,
    pub rows: Vec<HashMap<String, String>>,
    pub row_count: usize,
    pub total_balance: f64,
}

// TODO: this should be removed. This should be handled by script execution.
pub fn parse_csv_content(path: &Path, content: &str) -> Result<ParsedCsv, SkillError> {
    let mut lines = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());

    let Some(header_line) = lines.next() else {
        return Err(SkillError::CsvEmpty(path.display().to_string()));
    };

    let headers: Vec<String> = header_line
        .split(',')
        .map(|value| value.trim().to_string())
        .collect();

    if headers.is_empty() {
        return Err(SkillError::CsvEmpty(format!(
            "no headers in {}",
            path.display()
        )));
    }

    let mut rows = Vec::new();
    let mut total_balance = 0.0_f64;
    for line in lines {
        let values: Vec<String> = line
            .split(',')
            .map(|value| value.trim().to_string())
            .collect();

        if values.len() != headers.len() {
            return Err(SkillError::CsvColumnMismatch {
                expected: headers.len(),
                actual: values.len(),
                path: path.display().to_string(),
            });
        }

        let mut row = HashMap::new();
        for (idx, header) in headers.iter().enumerate() {
            let value = values[idx].clone();
            if header.eq_ignore_ascii_case("balance") {
                total_balance += parse_numeric_value(&value);
            }
            row.insert(header.clone(), value);
        }
        rows.push(row);
    }

    Ok(ParsedCsv {
        row_count: rows.len(),
        headers,
        rows,
        total_balance,
    })
}

fn parse_numeric_value(value: &str) -> f64 {
    let cleaned: String = value
        .chars()
        .filter(|ch| ch.is_ascii_digit() || *ch == '.' || *ch == '-')
        .collect();
    cleaned.parse::<f64>().unwrap_or(0.0)
}
