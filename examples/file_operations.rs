//! Async file I/O: create, read, write, rename, and remove.
//!
//! ```text
//! cargo run --example file_operations
//! ```

use std::path::PathBuf;

#[karmaio::main]
async fn main() -> std::io::Result<()> {
    let path = PathBuf::from("example_file.txt");
    let new_path = PathBuf::from("example_file_renamed.txt");

    // Write to a file
    println!("Writing to file...");
    karmaio::fs::write(&path, b"Hello, karmaio!\nThis is an async file operation.").await?;
    println!("File written successfully.");

    // Read from a file
    println!("\nReading from file...");
    let contents = karmaio::fs::read(&path).await?;
    println!("File contents: {}", String::from_utf8_lossy(&contents));

    // Rename the file
    println!("\nRenaming file...");
    karmaio::fs::rename(&path, &new_path).await?;
    println!("File renamed successfully.");

    // Read from the renamed file
    println!("\nReading from renamed file...");
    let contents = karmaio::fs::read(&new_path).await?;
    println!("Renamed file contents: {}", String::from_utf8_lossy(&contents));

    // Remove the file
    println!("\nRemoving file...");
    karmaio::fs::remove_file(&new_path).await?;
    println!("File removed successfully.");

    // Verify the file is removed
    match karmaio::fs::read(&new_path).await {
        Ok(_) => println!("File still exists (unexpected)!"),
        Err(e) => println!("File properly removed (expected error: {e})"),
    }

    Ok(())
}
