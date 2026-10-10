use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
#[derive(Parser)]
#[command(about = "Convert an Arch .pkg.tar.zst archive without executing its scripts")]
struct Cli {
    input: PathBuf,
    output: PathBuf,
}
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
fn run() -> Result<()> {
    let args = Cli::parse();
    let p = maple::convert::convert(&args.input, &args.output)?;
    #[derive(serde::Serialize)]
    struct Entry<'a> {
        package: [&'a maple::model::Package; 1],
    }
    println!("{}", toml::to_string(&Entry { package: [&p] })?);
    println!(
        "[sha256]\n{:?} = {:?}",
        format!("{}/{}", p.name, p.version),
        maple::repository::sha256(&args.output)?
    );
    Ok(())
}
