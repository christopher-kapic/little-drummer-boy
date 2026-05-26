//! Startup welcome art shown when launching the interactive TUI via bare `cockpit`.
//!
//! - Default: the P-51 Mustang (p51-4.sh)
//! - COCKPIT_MEME=1: the rooster (rooster-4.sh)
//!
//! The strings below are the exact stdout of those scripts (ANSI + quadrant blocks).
//! To update after editing the .sh files:
//!     bash p51-4.sh > /tmp/p51 && bash rooster-4.sh > /tmp/rooster
//! Then paste the contents into the raw strings here (or add a build.rs later).

/// Rendered output of p51-4.sh (default welcome).
pub const P51: &str = r#"
    [38;5;255m█[0m   [38;5;196;48;5;16m▖[0m[38;5;250;48;5;244m▖[0m
    [38;5;255m▐[0m[38;5;255;48;5;250m▌[0m[38;5;250m▄[0m[38;5;250;48;5;244m▛[0m[38;5;250;48;5;244m▘[0m 
    [38;5;33;48;5;45m▖[0m[38;5;250;48;5;255m▛[0m[38;5;250;48;5;255m▀[0m   
  [38;5;220;48;5;208m▖[0m[38;5;220;48;5;208m▘[0m[38;5;208;48;5;250m▘[0m[38;5;244m▘[0m[38;5;255m▜[0m[38;5;255m▖[0m  

"#;

/// Rendered output of rooster-4.sh (when COCKPIT_MEME=1).
pub const ROOSTER: &str = r#"
  [38;5;196m▗[0m[38;5;196;48;5;220m▝[0m[38;5;196;48;5;16m▘[0m[38;5;196m█[0m[38;5;33m▄[0m   
  [38;5;16;48;5;220m▘[0m[38;5;220;48;5;208m▛[0m[38;5;220;48;5;208m▘[0m[38;5;196m█[0m[38;5;33;48;5;45m▘[0m[38;5;45;48;5;33m▙[0m[38;5;45;48;5;18m▖[0m 
  [38;5;124;48;5;196m▚[0m[38;5;196;48;5;208m▙[0m[38;5;208;48;5;196m▘[0m[38;5;196m█[0m[38;5;196m█[0m[38;5;208;48;5;45m▘[0m[38;5;45;48;5;18m▌[0m 
   [38;5;94;48;5;220m▝[0m[38;5;94m▘[0m[38;5;94;48;5;220m▀[0m[38;5;22m▗[0m[38;5;34;48;5;22m▖[0m  

"#;

/// Print the appropriate welcome art to stdout.
///
/// Respects `COCKPIT_MEME=1` to select the rooster variant.
pub fn print() {
    let art = if std::env::var("COCKPIT_MEME")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        ROOSTER
    } else {
        P51
    };
    // Print exactly the captured bytes (scripts already provide leading + trailing newlines).
    print!("{}", art);
}
