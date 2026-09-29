//! Prints the readable block of a receiver bug report. Takes the blob, an
//! issue body around it, or the new-issue link, from the arguments or stdin.
//! Built from the same tree as the receiver that made the blob.
//!
//! cargo run -q -p fcast-bug-report --bin fcast-bug-report-decode -- <blob>

use std::io::Read;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let text = if args.is_empty() {
        let mut text = String::new();
        if let Err(err) = std::io::stdin().read_to_string(&mut text) {
            eprintln!("stdin: {err}");
            return ExitCode::FAILURE;
        }
        text
    } else {
        args.join(" ")
    };
    match fcast_bug_report::find_and_decode(&text) {
        Ok(report) => {
            println!("{}", report.render());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("decode failed: {err}");
            ExitCode::FAILURE
        }
    }
}
