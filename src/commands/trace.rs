use std::fs;

use crate::backtracer;

pub fn cmd_trace() -> Result<(), String> {
    let path = backtracer::log_path();
    if !path.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    print!("{}", content);
    println!();
    Ok(())
}
