//! Async process execution: spawn, pipe stdin/stdout, capture output.
//!
//! ```text
//! cargo run --example process
//! ```

use karmaio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use karmaio::process::{Command, Stdio as KarmaioStdio};

#[karmaio::main]
async fn main() -> std::io::Result<()> {
    // Example 1: Simple command execution
    println!("Example 1: Simple command execution");
    let status = Command::new("echo").arg("Hello from karmaio!").status().await?;
    println!("Exit status: {status}");

    // Example 2: Capture command output
    println!("\nExample 2: Capture command output");
    let output = Command::new("echo").arg("This is captured output").output().await?;
    println!("Exit status: {}", output.status);
    println!("Stdout: {}", String::from_utf8_lossy(&output.stdout));
    println!("Stderr: {}", String::from_utf8_lossy(&output.stderr));

    // Example 3: Piped stdin/stdout
    println!("\nExample 3: Piped stdin/stdout");
    let mut child = Command::new("cat")
        .stdin(KarmaioStdio::piped())
        .stdout(KarmaioStdio::piped())
        .spawn()?;

    // Write to stdin
    let mut stdin = child.take_stdin().expect("stdin should be piped");
    let (result, _) = stdin.write_all(b"Hello from cat!".to_vec()).await.into_parts();
    result?;
    stdin.shutdown().await?;

    // Read from stdout
    let mut stdout = child.take_stdout().expect("stdout should be piped");
    let buf = vec![0; 1024];
    let (result, buf) = stdout.read(buf).await.into_parts();
    let n = result?;
    println!("Read {n} bytes: {}", String::from_utf8_lossy(&buf[..n]));

    // Wait for process to complete
    let status = child.wait().await?;
    println!("Exit status: {status}");

    // Example 4: Run a command that fails
    println!("\nExample 4: Command that fails");
    let output = Command::new("ls").arg("/nonexistent_path").output().await?;
    println!("Exit status: {}", output.status);
    println!("Stderr: {}", String::from_utf8_lossy(&output.stderr));

    println!("\nAll process examples completed!");
    Ok(())
}
