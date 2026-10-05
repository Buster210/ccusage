use std::process::Command;

use ccusage_test_support::{Fixture, fs_fixture};

const SINCE: &str = "20260101";
const UNTIL: &str = "20260228";

#[test]
fn snapshots_copilot_focused_daily_stdout() {
    let fixture = copilot_fixture();

    insta::assert_snapshot!(
        "focused_daily_json",
        format!(
            "Daily JSON\n{}",
            run_cli(&fixture, ["copilot", "daily", "--json"]),
        )
    );
    insta::assert_snapshot!(
        "focused_daily_table",
        format!("Daily\n{}", run_cli(&fixture, ["copilot", "daily"]),)
    );
}

#[test]
fn snapshots_copilot_focused_monthly_and_session_stdout() {
    let fixture = copilot_fixture();

    insta::assert_snapshot!(
        "focused_monthly_and_session_json",
        format!(
            "Monthly JSON\n{}\nSession JSON\n{}",
            run_cli(&fixture, ["copilot", "monthly", "--json"]),
            run_cli(&fixture, ["copilot", "session", "--json"]),
        )
    );
    insta::assert_snapshot!(
        "focused_monthly_and_session_table",
        format!(
            "Monthly\n{}\n\nSession\n{}",
            run_cli(&fixture, ["copilot", "monthly"]),
            run_cli(&fixture, ["copilot", "session"]),
        )
    );
}

#[test]
fn snapshots_copilot_unified_monthly_and_session_stdout() {
    let fixture = copilot_fixture();

    insta::assert_snapshot!(
        "unified_monthly_and_session_json",
        format!(
            "Monthly JSON\n{}\nSession JSON\n{}",
            run_cli(&fixture, ["monthly", "--json"]),
            run_cli(&fixture, ["session", "--json"]),
        )
    );
    insta::assert_snapshot!(
        "unified_monthly_and_session_table",
        format!(
            "Monthly\n{}\n\nSession\n{}",
            run_cli(&fixture, ["monthly"]),
            run_cli(&fixture, ["session"]),
        )
    );
}

#[test]
fn unified_agent_totals_center_all_without_realigning_other_agents() {
    let fixture = copilot_fixture();

    for output in [
        run_cli(&fixture, ["monthly"]),
        run_cli(&fixture, ["monthly", "--compact"]),
    ] {
        let agent_cells = output
            .lines()
            .filter(|line| line.starts_with('│'))
            .filter_map(|line| line.split('│').nth(2))
            .collect::<Vec<_>>();
        let totals = agent_cells
            .iter()
            .filter(|cell| cell.trim() == "All")
            .collect::<Vec<_>>();
        assert!(!totals.is_empty(), "{output}");
        for cell in totals {
            let left = cell.len() - cell.trim_start().len();
            let right = cell.len() - cell.trim_end().len();
            assert!(left > 1 && left.abs_diff(right) <= 1, "{cell:?}");
        }
        let agents = agent_cells
            .iter()
            .filter(|cell| cell.contains("GitHub") || cell.contains("Copilot CLI"))
            .collect::<Vec<_>>();
        assert!(!agents.is_empty(), "{output}");
        for cell in agents {
            assert_eq!(cell.len() - cell.trim_start().len(), 1, "{cell:?}");
        }
    }
}

#[test]
fn unified_cost_column_labels_currency_once() {
    let fixture = copilot_fixture();

    for output in [
        run_cli(&fixture, ["monthly"]),
        run_cli(&fixture, ["monthly", "--compact"]),
        run_cli(&fixture, ["monthly", "--breakdown"]),
        run_cli(&fixture, ["monthly", "--compact", "--breakdown"]),
    ] {
        let table_lines = output
            .lines()
            .filter(|line| line.starts_with('│'))
            .collect::<Vec<_>>();
        let header = table_lines.first().expect("table should have a header");
        let cost_column = header
            .split('│')
            .position(|cell| cell.trim() == "Cost($)")
            .expect("unified cost header should label the currency once");
        let costs = table_lines
            .iter()
            .skip(1)
            .filter_map(|line| {
                let cost = line.split('│').nth(cost_column).map(str::trim)?;
                if cost.is_empty() {
                    return None;
                }
                let is_total = line
                    .split('│')
                    .nth(1)
                    .is_some_and(|cell| cell.contains("Total"));
                Some((cost.to_string(), is_total))
            })
            .collect::<Vec<_>>();
        assert!(!costs.is_empty(), "{output}");
        for (cost, is_total) in costs {
            let amount = if is_total {
                cost.strip_prefix('$')
                    .expect("total cost should keep the dollar sign")
            } else {
                assert!(!cost.starts_with('$'), "{cost:?}");
                cost.as_str()
            };
            assert!(amount.parse::<f64>().is_ok(), "{cost:?}");
            let (_, decimals) = amount.split_once('.').expect("cost should have decimals");
            assert_eq!(decimals.len(), 2, "{cost:?}");
        }
    }
}

#[test]
fn unified_table_preserves_model_names_by_tightening_numeric_columns() {
    let fixture = copilot_fixture();
    let _ = fixture.write_file(
        "copilot/session-state/session-a/events.jsonl",
        include_str!(
            "../../../adapters/copilot/tests/fixtures/session-state/session-a/events.jsonl"
        )
        .replace("claude-opus-4.6-1m", "gpt-5.2-codex"),
    );

    let output = run_cli(&fixture, ["monthly"]);
    let lines = output
        .lines()
        .filter(|line| line.starts_with('│'))
        .collect::<Vec<_>>();
    assert!(
        lines.iter().any(|line| {
            line.split('│')
                .nth(3)
                .is_some_and(|cell| cell.trim() == "gpt-5.2-codex")
        }),
        "{output}"
    );
    assert!(lines.iter().all(|line| line.chars().count() <= 120));
    for line in lines.iter().filter(|line| line.contains("gpt-5.2-codex")) {
        for cell in line.split('│').skip(4).take(6) {
            assert_eq!(cell.len() - cell.trim_end().len(), 1, "{cell:?}");
        }
    }
}

#[test]
fn unified_summary_rows_are_bold_without_styling_agents_or_borders() {
    let fixture = copilot_fixture();

    for output in [
        run_cli_with_color(&fixture, ["monthly"], true),
        run_cli_with_color(&fixture, ["monthly", "--compact"], true),
    ] {
        let summary_lines = output
            .lines()
            .filter(|line| line.starts_with('│'))
            .filter(|line| {
                line.split('│')
                    .nth(1)
                    .is_some_and(|cell| cell.contains("Total"))
                    || line
                        .split('│')
                        .nth(2)
                        .is_some_and(|cell| cell.contains("All"))
            })
            .collect::<Vec<_>>();
        assert!(!summary_lines.is_empty(), "{output}");
        for line in summary_lines {
            let cells = line.split('│').filter(|cell| {
                !cell
                    .replace("\x1b[1m", "")
                    .replace("\x1b[33m", "")
                    .replace("\x1b[0m", "")
                    .trim()
                    .is_empty()
            });
            for cell in cells {
                assert!(cell.contains("\x1b[1m"), "{cell:?}");
                assert!(cell.trim_end().ends_with("\x1b[0m"), "{cell:?}");
            }
        }
        let agent_lines = output
            .lines()
            .filter(|line| line.starts_with('│') && line.contains("Copilot CLI"))
            .collect::<Vec<_>>();
        assert!(!agent_lines.is_empty(), "{output}");
        assert!(agent_lines.iter().all(|line| !line.contains("\x1b[1m")));
        assert!(
            output
                .lines()
                .filter(|line| line.starts_with(['┌', '├', '└']))
                .all(|line| !line.contains('\x1b'))
        );
    }
    assert!(!run_cli(&fixture, ["monthly"]).contains('\x1b'));
}

#[test]
fn unified_total_stays_bold_when_colors_are_disabled() {
    let fixture = copilot_fixture();
    let output = run_cli_with_color(&fixture, ["monthly", "--no-color"], true);
    let total = output
        .lines()
        .find(|line| {
            line.starts_with('│')
                && line
                    .split('│')
                    .nth(1)
                    .is_some_and(|cell| cell.contains("Total"))
        })
        .expect("table should include a Total row");

    assert!(total.contains("\x1b[1mTotal\x1b[0m"), "{total:?}");
    assert!(!output.contains("\x1b[33m"));
    assert!(!output.contains("\x1b[34m"));
}

#[test]
fn unified_headers_are_bold_in_full_compact_and_monochrome_tables() {
    let fixture = copilot_fixture();

    for output in [
        run_cli_with_color(&fixture, ["monthly"], true),
        run_cli_with_color(&fixture, ["monthly", "--compact"], true),
        run_cli_with_color(&fixture, ["monthly", "--no-color"], true),
    ] {
        let header = output
            .lines()
            .find(|line| line.starts_with('│'))
            .expect("table should have a header");
        for cell in header.split('│').skip(1).filter(|cell| !cell.is_empty()) {
            assert!(cell.contains("\x1b[1m"), "{cell:?}");
            assert!(cell.trim_end().ends_with("\x1b[0m"), "{cell:?}");
        }
    }
}

fn copilot_fixture() -> Fixture {
    fs_fixture!({
        "copilot/session-state/session-a/events.jsonl": include_str!(
            "../../../adapters/copilot/tests/fixtures/session-state/session-a/events.jsonl"
        ),
        "copilot/session-state/session-b/events.jsonl": include_str!(
            "../../../adapters/copilot/tests/fixtures/session-state/session-b/events.jsonl"
        ),
        "copilot/otel/trace.jsonl": include_str!(
            "../../../adapters/copilot/tests/fixtures/otel/trace.jsonl"
        ),
    })
}

fn run_cli<const N: usize>(fixture: &Fixture, args: [&str; N]) -> String {
    run_cli_with_color(fixture, args, false)
}

fn run_cli_with_color<const N: usize>(fixture: &Fixture, args: [&str; N], color: bool) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ccusage"));
    command
        .args(args)
        .args(["--offline", "--timezone", "UTC"])
        .args(["--since", SINCE, "--until", UNTIL])
        .env("COPILOT_HOME", fixture.path("copilot"))
        .env("HOME", fixture.path("empty-home"))
        .env("USERPROFILE", fixture.path("empty-userprofile"))
        .env("XDG_CONFIG_HOME", fixture.path("empty-xdg-config"))
        .env("LOG_LEVEL", "0")
        .env("NO_COLOR", "1")
        .env("COLUMNS", "120")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("OPENCODE_DATA_DIR")
        .env_remove("AMP_DATA_DIR")
        .env_remove("DROID_SESSIONS_DIR")
        .env_remove("CODEBUFF_DATA_DIR")
        .env_remove("HERMES_HOME")
        .env_remove("PI_AGENT_DIR")
        .env_remove("GOOSE_PATH_ROOT")
        .env_remove("OPENCLAW_DIR")
        .env_remove("KILO_DATA_DIR")
        .env_remove("COPILOT_OTEL_FILE_EXPORTER_PATH")
        .env_remove("GEMINI_DATA_DIR")
        .env_remove("KIMI_DATA_DIR")
        .env_remove("QWEN_DATA_DIR")
        .env_remove("GROK_HOME");
    if color {
        command.env_remove("NO_COLOR").env("FORCE_COLOR", "1");
    } else {
        command.arg("--no-color").env_remove("FORCE_COLOR");
    }
    let output = command.output().expect("ccusage CLI should run");
    assert!(
        output.status.success(),
        "ccusage CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("ccusage CLI stdout should be UTF-8")
}
