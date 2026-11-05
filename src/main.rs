use clap::Parser;

fn main() {
    let cli = rex_relay::cli::Cli::parse();
    std::process::exit(rex_relay::commands::run(&cli));
}
