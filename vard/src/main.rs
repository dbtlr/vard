mod paths;

/// The CLI surface is not built yet; until it lands the binary identifies
/// itself and reports where it will keep its files.
fn main() {
    println!("vard {}", env!("CARGO_PKG_VERSION"));
    println!("config  {}", paths::config_file().display());
    println!("state   {}", paths::state_dir().display());
    println!("data    {}", paths::data_dir().display());
    println!("logs    {}", paths::log_dir().display());
}
