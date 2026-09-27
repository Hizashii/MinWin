use clap::{Parser, Subcommand};

use crate::{benchmark, system};

#[derive(Debug, Parser)]
#[command(name = "minwin", version, about = "Windows performance control center")]
pub struct Arguments {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Display the current system status.
    Status,
    /// Run the read-only benchmark placeholder.
    Benchmark,
    /// Run the read-only system scan placeholder.
    Scan,
}

impl Arguments {
    pub fn parse() -> Self {
        <Self as Parser>::parse()
    }
}

pub fn run(command: Command) -> anyhow::Result<()> {
    match command {
        Command::Status => print_status(),
        Command::Benchmark => print_benchmark(),
        Command::Scan => {
            println!("System scan placeholder — no system changes made.");
            println!("The scanner will inspect startup applications, services, scheduled tasks,");
            println!("installed applications, and unnecessary background activity.");
            Ok(())
        }
    }
}

fn print_status() -> anyhow::Result<()> {
    let status = system::status::get_system_status();

    println!("MinWin system status");
    println!("RAM: {}", format_percent(status.ram_usage_percent));
    println!("CPU: {}", format_percent(status.cpu_usage_percent));
    println!("Processes: {}", format_count(status.process_count));
    println!("Services: {}", format_count(status.service_count));
    println!("Uptime: {}", status.uptime.unwrap_or("--"));
    Ok(())
}

fn print_benchmark() -> anyhow::Result<()> {
    let results = benchmark::placeholder_results();

    println!("MinWin benchmark");
    println!("RAM: {}", results.ram);
    println!("CPU: {}", results.cpu);
    println!("Processes: {}", results.processes);
    println!("Services: {}", results.services);
    println!("Boot time: {}", results.boot_time);
    Ok(())
}

fn format_percent(value: Option<u8>) -> String {
    value
        .map(|percent| format!("{percent}%"))
        .unwrap_or_else(|| "-- (placeholder)".to_string())
}

fn format_count(value: Option<u32>) -> String {
    value
        .map(|count| count.to_string())
        .unwrap_or_else(|| "-- (placeholder)".to_string())
}
