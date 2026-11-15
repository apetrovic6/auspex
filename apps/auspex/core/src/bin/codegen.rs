use std::path::PathBuf;

use anyhow::Result;
use auspex_core::Auspex;
use clap::{Parser, ValueEnum};
use crux_core::type_generation::facet::{Config, TypeRegistry};
use log::info;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum Language {
    Swift,
    Kotlin,
    Csharp,
    Typescript,
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(short, long, value_enum)]
    language: Language,
    #[arg(short, long)]
    output_dir: PathBuf,
}

fn main() -> Result<()> {
    pretty_env_logger::init();
    let args = Args::parse();

    let typegen_app = TypeRegistry::new().register_app::<Auspex>()?.build()?;

    // The "name" a generator wants differs per language: a Swift/TypeScript module name,
    // a Kotlin package, a C# namespace. These must agree with boltffi.toml — the Kotlin
    // package in particular has to match [targets.android.kotlin].package, or the two
    // generators emit into different packages and Gradle compiles neither.
    let name = match args.language {
        Language::Swift => "App",
        Language::Kotlin => "com.example.auspex",
        Language::Csharp => "Auspex.Shared",
        Language::Typescript => "auspex",
    };
    let config = Config::builder(name, &args.output_dir).build();

    match args.language {
        Language::Swift => {
            info!("Typegen for Swift");
            typegen_app.swift(&config)?;
        }
        Language::Kotlin => {
            info!("Typegen for Kotlin");
            typegen_app.kotlin(&config)?;
        }
        Language::Csharp => {
            info!("Typegen for C#");
            typegen_app.csharp(&config)?;
        }
        Language::Typescript => {
            info!("Typegen for TypeScript");
            typegen_app.typescript(&config)?;
        }
    }

    Ok(())
}
