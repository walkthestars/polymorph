//! A scriptable stand-in for an SMT-LIB2 solver.
//!
//! The tests compile this file with `rustc` into a scratch directory and point
//! a `SolverSpec` at it, so every checker failure path — a timeout kill, an
//! `unknown`, garbage, a scrubbed environment, junk between batch markers —
//! runs deterministically and offline. No production path uses it.
//!
//! Usage: `fake_solver [flags] <input.smt2>`
//!
//! * `--answer sat|unsat|unknown|garbage|cycle|silent` — what to answer for
//!   every `@@ais <n>` item in the input (`cycle` = unsat, sat, unknown by
//!   index; `silent` = print nothing at all).
//! * `--sleep-ms <n>` — sleep before answering.
//! * `--echo-env <VAR>` — after each verdict, print `(:echo VAR "<value>")` or
//!   `(:echo VAR <unset>)`. Repeatable.
//! * `--preamble <line>` — print this line before each verdict (e.g. the
//!   `unsupported` a real solver answers to an option it does not know).
//! * `--echo-cwd` — after each verdict, print `(:cwd "<working directory>")`.
//! * `--junk` — print noise before the first marker, between items, and
//!   out-of-range or malformed markers.
//! * `--flood-bytes <n>` — write `n` bytes of noise to stdout and to stderr
//!   before answering, to fill the pipes.
//! * `--exit <code>` — exit with this code after answering.
//! * `-version` / `--version` — print a version line and exit.

use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
        .first()
        .is_some_and(|a| a == "-version" || a == "--version")
    {
        println!("fake-solver 1.0 (polymorph-ais tests)");
        return;
    }

    let mut answer = "unsat".to_string();
    let mut sleep_ms = 0u64;
    let mut echo_env: Vec<String> = Vec::new();
    let mut echo_cwd = false;
    let mut preamble: Option<String> = None;
    let mut junk = false;
    let mut flood = 0usize;
    let mut exit = 0i32;
    let mut input: Option<String> = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--answer" => answer = it.next().expect("--answer needs a value"),
            "--sleep-ms" => sleep_ms = it.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            "--echo-env" => echo_env.push(it.next().expect("--echo-env needs a name")),
            "--echo-cwd" => echo_cwd = true,
            "--preamble" => preamble = it.next(),
            "--junk" => junk = true,
            "--flood-bytes" => flood = it.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            "--exit" => exit = it.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            other => input = Some(other.to_string()),
        }
    }
    let input = input.expect("an input file is required");
    let text = std::fs::read_to_string(&input).expect("the input file is readable");

    if sleep_ms > 0 {
        std::thread::sleep(Duration::from_millis(sleep_ms));
    }
    if flood > 0 {
        let line = "; flood flood flood flood flood flood flood flood flood flood\n";
        let mut out = String::with_capacity(flood + line.len());
        while out.len() < flood {
            out.push_str(line);
        }
        eprint!("{out}");
        print!("{out}");
    }
    if answer == "silent" {
        std::process::exit(exit);
    }

    if junk {
        println!("WARNING: this line precedes every marker and belongs to no item");
        println!("@@ais not-a-number");
    }
    // A standalone script (the certificate replay) carries no marker and gets
    // no marker back, exactly like a real solver.
    let batched = text.contains("(echo \"@@ais ");
    for (index, item) in items(&text) {
        if batched {
            println!("@@ais {index}");
        }
        if let Some(line) = &preamble {
            println!("{line}");
        }
        let verdict = match answer.as_str() {
            "cycle" => ["unsat", "sat", "unknown"][index % 3],
            other => other,
        };
        match verdict {
            "garbage" => println!("(error \"line 1 column 1: this is not a verdict\")"),
            v => println!("{v}"),
        }
        if verdict == "sat" {
            let symbols = get_value_symbols(&item);
            if !symbols.is_empty() {
                let pairs: Vec<String> = symbols.iter().map(|s| format!("({s} 0)")).collect();
                println!("({})", pairs.join(" "));
            }
        }
        if verdict == "unknown" {
            println!("(:reason-unknown \"fake\")");
        }
        for var in &echo_env {
            match std::env::var(var) {
                Ok(v) => println!("(:echo {var} \"{v}\")"),
                Err(_) => println!("(:echo {var} <unset>)"),
            }
        }
        if echo_cwd {
            let cwd = std::env::current_dir().expect("a working directory");
            println!("(:cwd \"{}\")", cwd.display());
        }
        if junk {
            println!("; junk after item {index}");
            println!("@@ais 9999");
            println!("sat");
        }
    }
    std::process::exit(exit);
}

/// Split the batch input into `(index, item text)` on its
/// `(echo "@@ais <n>")` markers. A standalone script with no marker is one
/// item with index 0.
fn items(text: &str) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("(echo \"@@ais ") {
            if let Ok(i) = rest.trim_end_matches("\")").trim().parse::<usize>() {
                out.push((i, String::new()));
                continue;
            }
        }
        if let Some((_, body)) = out.last_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if out.is_empty() {
        out.push((0, text.to_string()));
    }
    out
}

/// The symbols of the item's `(get-value (a b c))` request, if any.
fn get_value_symbols(item: &str) -> Vec<String> {
    let Some(start) = item.find("(get-value (") else {
        return Vec::new();
    };
    let rest = &item[start + "(get-value (".len()..];
    let Some(end) = rest.find(')') else {
        return Vec::new();
    };
    rest[..end].split_whitespace().map(str::to_string).collect()
}
