use clap::{Parser, ValueEnum};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    version,
    about = "Inspect saved Windows task schedules and compare snapshots"
)]
struct Args {
    #[arg(long, help = "Input task snapshot in CSV format")]
    input: PathBuf,
    #[arg(
        long,
        conflicts_with = "summary",
        help = "Earlier CSV snapshot to compare with the input"
    )]
    compare: Option<PathBuf>,
    #[arg(
        long,
        help = "Task name pattern, using case-insensitive * and ? wildcards"
    )]
    name: Option<String>,
    #[arg(long, help = "Include this status (case-insensitive); may be repeated")]
    status: Vec<String>,
    #[arg(long, value_enum, default_value = "table", help = "Output format")]
    format: Format,
    #[arg(long, help = "Count matching tasks by status")]
    summary: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Table,
    Csv,
}

#[derive(Clone, Copy)]
enum State {
    Start,
    Bare,
    Quoted,
    Closed,
}

fn parse_csv(text: &str) -> Result<Vec<Vec<String>>, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut chars = text.chars().peekable();
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut state = State::Start;
    let mut pending = false;
    let mut line = 1;
    while let Some(ch) = chars.next() {
        pending = true;
        if matches!(state, State::Quoted) {
            if ch == '"' {
                state = State::Closed;
            } else {
                field.push(ch);
                if ch == '\n' {
                    line += 1;
                }
            }
            continue;
        }
        match ch {
            '"' if matches!(state, State::Start) => state = State::Quoted,
            '"' if matches!(state, State::Closed) => {
                field.push('"');
                state = State::Quoted;
            }
            ',' => {
                row.push(std::mem::take(&mut field));
                state = State::Start;
            }
            '\r' | '\n' => {
                if ch == '\r' && chars.next_if_eq(&'\n').is_none() {
                    return Err(format!("invalid CSV at line {line}: bare carriage return"));
                }
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                state = State::Start;
                pending = false;
                line += 1;
            }
            _ if matches!(state, State::Closed) => {
                return Err(format!(
                    "invalid CSV at line {line}: unexpected character after closing quote"
                ));
            }
            '"' => {
                return Err(format!(
                    "invalid CSV at line {line}: quote in unquoted field"
                ));
            }
            _ => {
                field.push(ch);
                state = State::Bare;
            }
        }
    }
    if matches!(state, State::Quoted) {
        return Err(format!(
            "invalid CSV at line {line}: unterminated quoted field"
        ));
    }
    if pending {
        row.push(field);
        rows.push(row);
    }
    Ok(rows)
}

struct Snapshot {
    headers: Vec<String>,
    tasks: BTreeMap<String, BTreeMap<String, String>>,
}

impl Snapshot {
    fn parse(text: &str) -> Result<Self, String> {
        let mut rows = parse_csv(text)?.into_iter();
        let headers = rows.next().ok_or("missing CSV header row")?;
        let mut seen = BTreeSet::new();
        for header in &headers {
            if header.is_empty() {
                return Err("empty CSV header".into());
            }
            if !seen.insert(header.as_str()) {
                return Err(format!("duplicate CSV header: {header:?}"));
            }
        }
        for required in ["TaskName", "Status", "Next Run Time"] {
            if !seen.contains(required) {
                return Err(format!("missing required header: {required}"));
            }
        }
        let mut tasks = BTreeMap::new();
        for (index, row) in rows.enumerate() {
            if row.len() != headers.len() {
                return Err(format!(
                    "invalid CSV record {}: expected {} fields, found {}",
                    index + 2,
                    headers.len(),
                    row.len()
                ));
            }
            let fields: BTreeMap<_, _> = headers.iter().cloned().zip(row).collect();
            let name = fields["TaskName"].clone();
            if name.is_empty() {
                return Err(format!("empty task name in CSV record {}", index + 2));
            }
            if tasks.insert(name.clone(), fields).is_some() {
                return Err(format!("duplicate task name: {name:?}"));
            }
        }
        Ok(Self { headers, tasks })
    }

    fn read(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Self::parse(&text).map_err(|error| format!("{}: {error}", path.display()))
    }
}

fn wildcard(pattern: &str, name: &str) -> bool {
    let pattern: Vec<_> = pattern.to_lowercase().chars().collect();
    let name: Vec<_> = name.to_lowercase().chars().collect();
    let (mut p, mut n, mut star, mut retry) = (0, 0, None, 0);
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            p += 1;
            retry = n;
        } else if let Some(position) = star {
            retry += 1;
            n = retry;
            p = position + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

fn matches(args: &Args, fields: &BTreeMap<String, String>) -> bool {
    args.name
        .as_ref()
        .is_none_or(|pattern| wildcard(pattern, &fields["TaskName"]))
        && (args.status.is_empty()
            || args
                .status
                .iter()
                .any(|status| status.to_lowercase() == fields["Status"].to_lowercase()))
}

fn compare(args: &Args, old: &Snapshot, new: &Snapshot) -> Vec<Vec<String>> {
    let names: BTreeSet<_> = old.tasks.keys().chain(new.tasks.keys()).collect();
    let mut rows = Vec::new();
    for name in names {
        let before = old.tasks.get(name);
        let after = new.tasks.get(name);
        if !before.is_some_and(|fields| matches(args, fields))
            && !after.is_some_and(|fields| matches(args, fields))
        {
            continue;
        }
        let change = match (before, after) {
            (None, Some(_)) => "Added",
            (Some(_), None) => "Removed",
            _ => "Changed",
        };
        let fields: BTreeSet<_> = before
            .into_iter()
            .chain(after)
            .flat_map(|fields| fields.keys())
            .collect();
        for field in fields {
            let old_value = before.and_then(|fields| fields.get(field));
            let new_value = after.and_then(|fields| fields.get(field));
            if old_value != new_value {
                rows.push(vec![
                    change.into(),
                    name.clone(),
                    field.clone(),
                    old_value.cloned().unwrap_or_default(),
                    new_value.cloned().unwrap_or_default(),
                ]);
            }
        }
    }
    rows
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.into()
    }
}

fn table_field(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '\n' => "\\n".into(),
            '\r' => "\\r".into(),
            '\t' => "\\t".into(),
            ch if ch.is_control() => ch.escape_default().to_string(),
            ch => ch.to_string(),
        })
        .collect()
}

fn render(headers: Vec<String>, rows: Vec<Vec<String>>, format: Format) -> String {
    let rows: Vec<_> = std::iter::once(headers).chain(rows).collect();
    match format {
        Format::Csv => rows
            .iter()
            .map(|row| {
                format!(
                    "{}\r\n",
                    row.iter()
                        .map(|value| csv_field(value))
                        .collect::<Vec<_>>()
                        .join(",")
                )
            })
            .collect(),
        Format::Table => {
            let rows: Vec<Vec<_>> = rows
                .iter()
                .map(|row| row.iter().map(|value| table_field(value)).collect())
                .collect();
            let widths: Vec<_> = (0..rows[0].len())
                .map(|index| {
                    rows.iter()
                        .map(|row| row[index].chars().count())
                        .max()
                        .unwrap_or(0)
                })
                .collect();
            rows.iter()
                .map(|row| {
                    let last = row.len() - 1;
                    let cells: Vec<_> = row
                        .iter()
                        .enumerate()
                        .map(|(index, value)| {
                            if index == last {
                                value.clone()
                            } else {
                                format!(
                                    "{value}{}",
                                    " ".repeat(widths[index] - value.chars().count())
                                )
                            }
                        })
                        .collect();
                    format!("{}\n", cells.join("  "))
                })
                .collect()
        }
    }
}

fn run(args: Args) -> Result<(), String> {
    let snapshot = Snapshot::read(&args.input)?;
    let (headers, rows) = if let Some(path) = &args.compare {
        let old = Snapshot::read(path)?;
        (
            vec!["Change", "TaskName", "Field", "Old Value", "New Value"]
                .into_iter()
                .map(String::from)
                .collect(),
            compare(&args, &old, &snapshot),
        )
    } else if args.summary {
        let mut counts: BTreeMap<String, (String, usize)> = BTreeMap::new();
        for fields in snapshot
            .tasks
            .values()
            .filter(|fields| matches(&args, fields))
        {
            let status = &fields["Status"];
            let entry = counts
                .entry(status.to_lowercase())
                .or_insert((status.clone(), 0));
            if status < &entry.0 {
                entry.0 = status.clone();
            }
            entry.1 += 1;
        }
        (
            vec!["Status".into(), "Count".into()],
            counts
                .into_values()
                .map(|(status, count)| vec![status, count.to_string()])
                .collect(),
        )
    } else {
        let rows = snapshot
            .tasks
            .values()
            .filter(|fields| matches(&args, fields))
            .map(|fields| {
                snapshot
                    .headers
                    .iter()
                    .map(|header| fields[header].clone())
                    .collect()
            })
            .collect();
        (snapshot.headers.clone(), rows)
    };
    io::stdout()
        .lock()
        .write_all(render(headers, rows, args.format).as_bytes())
        .map_err(|error| format!("cannot write output: {error}"))
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "TaskName,Status,Next Run Time\r\n";

    #[test]
    fn quoted_fields_bom_and_embedded_newlines() {
        let rows = parse_csv("\u{feff}\"a,b\",\"say \"\"hello\"\"\",\"line\r\nnext\"\r\n").unwrap();
        assert_eq!(rows, vec![vec!["a,b", "say \"hello\"", "line\r\nnext"]]);
    }

    #[test]
    fn malformed_csv_is_rejected() {
        for input in ["\"unfinished", "bare\"quote", "\"closed\"text", "a\rb"] {
            assert!(parse_csv(input).is_err(), "{input:?}");
        }
        assert_eq!(parse_csv("a,b,").unwrap(), vec![vec!["a", "b", ""]]);
        assert_eq!(parse_csv("\"\"").unwrap(), vec![vec![""]]);
    }

    #[test]
    fn required_headers_and_duplicate_tasks_are_checked() {
        assert!(
            Snapshot::parse("TaskName,Next Run Time\n\\Backup,N/A\n")
                .err()
                .expect("expected missing header")
                .contains("Status")
        );
        assert!(
            Snapshot::parse(&format!(
                "{HEADER}\\Backup,Ready,N/A\n\\Backup,Disabled,N/A\n"
            ))
            .is_err()
        );
        assert!(Snapshot::parse(&format!("{HEADER}\\Backup,Ready\n")).is_err());
        assert!(Snapshot::parse("TaskName,Status,Next Run Time,Status\n").is_err());
    }

    #[test]
    fn windows_patterns_match_case_insensitively() {
        assert!(wildcard("\\BACK*?", "\\Backup"));
        assert!(wildcard("*a*b", "AAAb"));
        assert!(wildcard("*", ""));
        assert!(wildcard("?", "é"));
        assert!(!wildcard("\\Back?", "\\Backup"));
        assert!(!wildcard("*a", "bbb"));
    }

    #[test]
    fn comparison_reports_added_removed_and_changed_fields() {
        let args = Args::try_parse_from(["schtasks", "--input", "new.csv"]).unwrap();
        let old = Snapshot::parse(&format!(
            "{HEADER}\\Changed,Ready,N/A\n\\Removed,Ready,N/A\n"
        ))
        .unwrap();
        let new = Snapshot::parse(&format!(
            "{HEADER}\\Added,Ready,N/A\n\\Changed,Disabled,N/A\n"
        ))
        .unwrap();
        let rows = compare(&args, &old, &new);
        assert_eq!(rows.len(), 7);
        assert!(rows.contains(&vec![
            "Changed".into(),
            "\\Changed".into(),
            "Status".into(),
            "Ready".into(),
            "Disabled".into()
        ]));
        assert_eq!(rows[0][0], "Added");
        assert_eq!(rows[6][0], "Removed");
    }

    #[test]
    fn csv_output_round_trips_and_preserves_extra_columns() {
        let snapshot = Snapshot::parse("TaskName,Status,Next Run Time,Details\n\\Task,Ready,locale date,\"a,\"\"b\"\"\nnext\"\n").unwrap();
        let fields = &snapshot.tasks["\\Task"];
        let row = snapshot
            .headers
            .iter()
            .map(|header| fields[header].clone())
            .collect();
        let output = render(snapshot.headers.clone(), vec![row], Format::Csv);
        let reparsed = Snapshot::parse(&output).unwrap();
        assert_eq!(snapshot.tasks, reparsed.tasks);
    }

    #[test]
    fn status_filters_and_cli_validation() {
        let args = Args::try_parse_from([
            "schtasks",
            "--input",
            "tasks.csv",
            "--status",
            "rEaDy",
            "--name",
            "\\B*",
        ])
        .unwrap();
        let snapshot = Snapshot::parse(&format!(
            "{HEADER}\\Backup,Ready,N/A\n\\Cleanup,Ready,N/A\n"
        ))
        .unwrap();
        assert!(matches(&args, &snapshot.tasks["\\Backup"]));
        assert!(!matches(&args, &snapshot.tasks["\\Cleanup"]));
        assert!(
            Args::try_parse_from(["schtasks", "--input", "tasks.csv", "--format", "json"]).is_err()
        );
        assert!(
            Args::try_parse_from([
                "schtasks",
                "--input",
                "tasks.csv",
                "--summary",
                "--compare",
                "old.csv"
            ])
            .is_err()
        );
    }
}
