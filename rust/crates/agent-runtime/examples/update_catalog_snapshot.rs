//! Offline update only. Accepts an already-exported model snapshot JSON file.
use agent_runtime::catalog::validate_snapshot;
use agent_runtime::images::parse_image_catalog;
use std::fs;
use std::path::PathBuf;

fn run() -> Result<(), String> {
    let mut args: Vec<_> = std::env::args_os().skip(1).collect();
    let images = args.first().is_some_and(|arg| arg == "--images");
    if images {
        args.remove(0);
    }
    if args.len() != 2 {
        return Err("Usage: update_catalog_snapshot [--images] INPUT.json OUTPUT.json (offline; retains OUTPUT.json.bak)".into());
    }
    let input = PathBuf::from(&args[0]);
    let output = PathBuf::from(&args[1]);
    let source = fs::read_to_string(&input).map_err(|error| error.to_string())?;
    let validate = |source: &str| {
        if images {
            parse_image_catalog(source).map(|models| models.len())
        } else {
            validate_snapshot(source)
        }
    };
    let count = validate(&source)?;
    let mut backup = output.as_os_str().to_os_string();
    backup.push(".bak");
    let backup = PathBuf::from(backup);
    let previous = if output.exists() {
        Some(fs::read(&output).map_err(|error| error.to_string())?)
    } else {
        None
    };
    if let Some(previous) = &previous {
        if backup.exists() {
            return Err("Backup already exists; archive it before updating the snapshot".into());
        }
        fs::write(&backup, previous).map_err(|error| error.to_string())?;
    }
    if let Err(error) = fs::write(&output, &source) {
        if let Some(previous) = &previous {
            let _ = fs::write(&output, previous);
        } else if output.exists() {
            let _ = fs::remove_file(&output);
        }
        return Err(format!("Snapshot update failed: {error}"));
    }
    let verification = fs::read_to_string(&output)
        .map_err(|error| error.to_string())
        .and_then(|written| {
            if written != source || validate(&written)? != count {
                return Err("Snapshot verification failed".to_owned());
            }
            Ok(())
        });
    if let Err(error) = verification {
        if let Some(previous) = &previous {
            fs::write(&output, previous).map_err(|error| error.to_string())?;
        } else {
            fs::remove_file(&output).map_err(|error| error.to_string())?;
        }
        return Err(format!("{error}; previous output restored"));
    }
    println!("Updated {count} models: {}", output.display());
    if previous.is_some() {
        println!("Rollback copy: {}", backup.display());
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
