use anyhow::Result;
use clap::{Parser, Subcommand};

use quiver_netlab::{Lab, probe, profile};

#[derive(Parser)]
#[command(name = "quiver-netlab", about = "Quiver's network-conditions lab")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Verify {
        #[arg(long)]
        profile: String,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Verify { profile: names } => {
            let lab = Lab::new()?;
            let list: Vec<String> = if names == "all" {
                profile::BUILTIN.iter().map(|(n, _)| n.to_string()).collect()
            } else {
                names.split(',').map(String::from).collect()
            };
            for name in list {
                let p = profile::load(&name)?;
                let v = probe::verify(&lab, &p, &probe::ProbeOptions::default())?;
                print!("{}", v.render());
            }
        }
    }
    Ok(())
}
