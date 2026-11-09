use clap::Parser;

use rscreen::cli::{main_entry, Cli};

fn main() {
    let cli = Cli::parse();
    let code = main_entry(cli);
    std::process::exit(code);
}
