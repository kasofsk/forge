use std::io::{self, BufRead, Write};

use anyhow::Result;
use async_trait::async_trait;
use workflow_types::Job;
use workflow_worker::{forgejo::ForgejoClient, worker::{Outcome, Worker}};

pub struct CliWorker {
    worker_id: String,
    title_filter: Option<String>,
}

impl CliWorker {
    pub fn new(worker_id: String, title_filter: Option<String>) -> Self {
        Self { worker_id, title_filter }
    }
}

#[async_trait]
impl Worker for CliWorker {
    fn worker_id(&self) -> &str {
        &self.worker_id
    }

    fn accepts(&self, job: &Job) -> bool {
        match &self.title_filter {
            Some(f) => job.title.to_lowercase().contains(&f.to_lowercase()),
            None => true,
        }
    }

    async fn execute(&self, job: &Job, _forgejo: &ForgejoClient) -> Result<Outcome> {
        Ok(interactive_session(job))
    }
}

pub fn interactive_session(job: &Job) -> Outcome {
    crate::print_job_detail(job);
    println!();
    println!("Heartbeat running in background.");
    println!();
    print_session_help();

    loop {
        print!("> ");
        let _ = io::stdout().flush();

        let mut line = String::new();
        match io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => return Outcome::Abandon, // EOF
            Ok(_) => {}
        }

        let input = line.trim();
        let parts: Vec<&str> = input.splitn(2, ' ').collect();

        match parts.as_slice() {
            [] => {}
            ["done"] | ["d"] => {
                return Outcome::Complete;
            }
            ["fail"] => {
                print!("Reason: ");
                let _ = io::stdout().flush();
                let mut reason = String::new();
                let _ = io::stdin().lock().read_line(&mut reason);
                let reason = reason.trim().to_string();
                let reason = if reason.is_empty() {
                    "no reason given".to_string()
                } else {
                    reason
                };
                return Outcome::Fail { reason, logs: None };
            }
            ["fail", reason] => {
                return Outcome::Fail {
                    reason: reason.to_string(),
                    logs: None,
                };
            }
            ["abandon"] | ["a"] => {
                return Outcome::Abandon;
            }
            ["show"] | ["s"] => {
                crate::print_job_detail(job);
            }
            ["help"] | ["h"] | ["?"] => {
                print_session_help();
            }
            [""] => {}
            [cmd, ..] => {
                println!("Unknown command: {cmd}  (type 'help' for commands)");
            }
        }
    }
}

fn print_session_help() {
    println!("Commands:");
    println!("  done              Mark job complete → in-review");
    println!("  fail [reason]     Mark job failed (prompts for reason if omitted)");
    println!("  abandon           Return job to on-deck");
    println!("  show              Print job details again");
    println!("  help              Show this message");
}
