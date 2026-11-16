//! Print the work board: tracked items arranged by status, the way a surface
//! draws them.
//!
//! The point is the data path, not the ASCII: `mb list --all --json` in,
//! columns out, with the columns nobody is working in collapsed to a count.
//! A wall drawing the same board reads the same projection.

use soil::work::WorkBoard;
use soil_marbles::MarblesClient;

fn main() -> anyhow::Result<()> {
    let mut binary = String::from("mb");
    let mut project: Option<String> = None;
    let mut per_column = 3usize;
    let mut collapsed: Vec<String> = vec!["open".into(), "deferred".into(), "closed".into()];
    let mut all = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--all" => all = true,
            "--binary" => binary = args.next().expect("--binary PATH"),
            "--project" => project = args.next(),
            "--per-column" => {
                per_column = args
                    .next()
                    .and_then(|n| n.parse().ok())
                    .expect("--per-column N")
            }
            "--expand" => {
                for status in args.next().expect("--expand a,b").split(',') {
                    collapsed.retain(|kept| kept != status.trim());
                }
            }
            "--collapse" => {
                collapsed = args
                    .next()
                    .expect("--collapse a,b")
                    .split(',')
                    .map(|status| status.trim().to_string())
                    .collect()
            }
            "--help" | "-h" => {
                println!(
                    "usage: soil-work [--binary PATH] [--project NAME] [--all]\n\
                     \x20                 [--per-column N] [--collapse a,b] [--expand a,b]"
                );
                return Ok(());
            }
            other => anyhow::bail!("unknown argument {other:?} (try --help)"),
        }
    }

    let mut client = MarblesClient::new(binary);
    if let Some(project) = project {
        client = client.with_project(project);
    }
    let issues = if all { client.list_all()? } else { client.list()? };
    let board = WorkBoard::project(&issues, per_column);

    println!(
        "work · {} items · {} in flight",
        board.total(),
        board.in_flight()
    );
    for column in &board.columns {
        if collapsed.contains(&column.status) {
            continue;
        }
        println!("\n{} ({})", column.status, column.total);
        for item in &column.items {
            let mut line = format!("  {:<18} {}", item.id, item.title);
            if let Some(assignee) = &item.assignee {
                line.push_str(&format!("  @{assignee}"));
            }
            println!("{line}");
        }
        let hidden = column.total.saturating_sub(column.items.len());
        if hidden > 0 {
            println!("  … {hidden} more");
        }
    }

    let folded: Vec<String> = board
        .columns
        .iter()
        .filter(|column| collapsed.contains(&column.status))
        .map(|column| format!("{} {}", column.status, column.total))
        .collect();
    if !folded.is_empty() {
        println!("\ncollapsed: {}", folded.join(" · "));
    }
    Ok(())
}
