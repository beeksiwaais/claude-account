use std::env;
use std::fs;
use std::io::{ErrorKind, IsTerminal};
use std::path::Path;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use chrono::DateTime;
use serde::Deserialize;

use crate::paths::AppPaths;
use crate::state;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const BAR_WIDTH: usize = 20;

#[derive(Debug, Deserialize)]
struct Credentials {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<OAuthToken>,
}

#[derive(Debug, Deserialize)]
struct OAuthToken {
    #[serde(rename = "accessToken")]
    access_token: String,
    /// Milliseconds since the Unix epoch.
    #[serde(rename = "expiresAt")]
    expires_at: Option<u64>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    five_hour: Option<Window>,
    seven_day: Option<Window>,
}

#[derive(Debug, Deserialize)]
struct Window {
    /// Percentage of the window already used, from 0 to 100.
    utilization: f64,
    resets_at: Option<String>,
}

struct Report {
    plan: Option<String>,
    usage: Usage,
}

/// Print the 5-hour and weekly subscription limits of every profile.
///
/// Each profile's OAuth token is read only for the duration of one HTTPS
/// request to Anthropic; it is never stored, logged, or refreshed here.
pub fn status(paths: &AppPaths) -> Result<()> {
    let state = state::load(paths)?;
    if state.profiles.is_empty() {
        println!("No profiles. Add one with `claude account add NAME`.");
        return Ok(());
    }
    let reports = fetch_reports(&state)?;

    let style = Style::detect();
    let now = now_secs();
    let name_width = reports
        .iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0);
    for (index, (name, report)) in reports.iter().enumerate() {
        if index > 0 {
            println!();
        }
        let active = state.active.as_deref() == Some(name.as_str());
        print!(
            "{}",
            render_profile(name, active, name_width, report, now, &style)
        );
    }
    Ok(())
}

/// Return the profile that can do the most work over the next five hours,
/// printing each profile's available share along the way.
pub fn best_profile(paths: &AppPaths) -> Result<String> {
    let state = state::load(paths)?;
    if state.profiles.is_empty() {
        bail!("no profiles; add one with `claude account add NAME`");
    }
    let reports = fetch_reports(&state)?;

    let name_width = reports
        .iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0);
    for (name, report) in &reports {
        match report {
            Ok(report) => println!(
                "  {name:<name_width$}  {:>3}% available",
                available(&report.usage).round() as u64
            ),
            Err(error) => println!("  {name:<name_width$}  skipped: {error:#}"),
        }
    }

    let candidates: Vec<(&str, f64)> = reports
        .iter()
        .filter_map(|(name, report)| {
            let report = report.as_ref().ok()?;
            Some((name.as_str(), available(&report.usage)))
        })
        .collect();
    pick_best(&candidates, state.active.as_deref())
        .map(str::to_owned)
        .context("no profile has any usage left; wait for a limit to reset")
}

/// The share of the 5-hour limit that can actually be used right now: the
/// weekly limit caps it, so an account with a fresh 5-hour window but no
/// weekly allowance left has nothing available.
fn available(usage: &Usage) -> f64 {
    let left = |window: Option<&Window>| {
        window.map_or(100.0, |window| 100.0 - window.utilization.clamp(0.0, 100.0))
    };
    left(usage.five_hour.as_ref()).min(left(usage.seven_day.as_ref()))
}

/// Pick the profile with the most available usage, preferring the active
/// profile and then the first name on ties, and ignoring exhausted profiles.
fn pick_best<'a>(candidates: &[(&'a str, f64)], active: Option<&str>) -> Option<&'a str> {
    candidates
        .iter()
        .filter(|(_, available)| *available > 0.0)
        .max_by(|(left_name, left), (right_name, right)| {
            left.total_cmp(right)
                .then_with(|| (Some(*left_name) == active).cmp(&(Some(*right_name) == active)))
                .then_with(|| right_name.cmp(left_name))
        })
        .map(|(name, _)| *name)
}

fn fetch_reports(state: &state::State) -> Result<Vec<(String, Result<Report>)>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("claude-account/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to create HTTP client")?;

    Ok(thread::scope(|scope| {
        let handles: Vec<_> = state
            .profiles
            .iter()
            .map(|(name, profile)| {
                let client = &client;
                let name = name.clone();
                let handle = scope.spawn(move || fetch_report(client, &profile.config_dir));
                (name, handle)
            })
            .collect();
        handles
            .into_iter()
            .map(|(name, handle)| {
                let report = handle
                    .join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("usage request panicked")));
                (name, report)
            })
            .collect()
    }))
}

fn fetch_report(client: &reqwest::blocking::Client, config_dir: &Path) -> Result<Report> {
    let token = read_token(config_dir)?;
    if token
        .expires_at
        .is_some_and(|expires_at| expires_at / 1000 <= now_secs())
    {
        bail!("login expired; run `claude` with this profile once to refresh it");
    }

    let response = client
        .get(env::var("CLAUDE_ACCOUNT_USAGE_URL").unwrap_or_else(|_| USAGE_URL.to_owned()))
        .bearer_auth(&token.access_token)
        .header("anthropic-beta", OAUTH_BETA)
        .send()
        .context("usage request failed")?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        bail!(
            "Anthropic rejected the login ({status}); run `claude` with this profile to refresh it"
        );
    }
    if !status.is_success() {
        bail!("Anthropic returned {status}");
    }
    let usage: Usage = response.json().context("invalid usage response")?;
    Ok(Report {
        plan: token.subscription_type,
        usage,
    })
}

fn read_token(config_dir: &Path) -> Result<OAuthToken> {
    let raw = match read_credentials_file(config_dir)? {
        Some(raw) => raw,
        None => read_keychain(config_dir)?,
    };
    parse_token(&raw)
}

fn parse_token(raw: &[u8]) -> Result<OAuthToken> {
    let credentials: Credentials =
        serde_json::from_slice(raw).context("Claude credentials have an unexpected format")?;
    credentials
        .oauth
        .context("profile is not logged in with a Claude subscription")
}

/// Claude Code stores credentials in this file on Linux, and on macOS when the
/// Keychain is unavailable.
fn read_credentials_file(config_dir: &Path) -> Result<Option<Vec<u8>>> {
    let path = config_dir.join(".credentials.json");
    match fs::read(&path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

#[cfg(target_os = "macos")]
fn read_keychain(config_dir: &Path) -> Result<Vec<u8>> {
    let service = keychain_service(config_dir);
    let output = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", &service, "-w"])
        .stderr(std::process::Stdio::null())
        .output()
        .context("failed to run /usr/bin/security")?;
    if !output.status.success() {
        bail!("no Claude login found in the Keychain; run `claude account add` again");
    }
    Ok(output.stdout)
}

#[cfg(not(target_os = "macos"))]
fn read_keychain(_config_dir: &Path) -> Result<Vec<u8>> {
    bail!("no Claude login found; run `claude account add` again")
}

/// Claude Code scopes its Keychain item to `CLAUDE_CONFIG_DIR` by suffixing the
/// service name with the first 8 hex digits of the directory's SHA-256.
#[cfg(any(target_os = "macos", test))]
fn keychain_service(config_dir: &Path) -> String {
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;

    let digest = Sha256::digest(config_dir.as_os_str().as_bytes());
    let hex: String = digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("Claude Code-credentials-{hex}")
}

struct Style {
    color: bool,
}

impl Style {
    fn detect() -> Self {
        Self {
            color: std::io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none(),
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
}

fn render_profile(
    name: &str,
    active: bool,
    name_width: usize,
    report: &Result<Report>,
    now: u64,
    style: &Style,
) -> String {
    let marker = if active { "*" } else { " " };
    let padded = format!("{name:<name_width$}");
    let mut header = format!("{marker} {}", style.paint("1", &padded));
    let mut out = String::new();
    match report {
        Ok(report) => {
            if let Some(plan) = &report.plan {
                header.push_str(&format!("  {}", style.paint("2", plan)));
            }
            out.push_str(&header);
            out.push('\n');
            out.push_str(&render_window(
                "5h",
                report.usage.five_hour.as_ref(),
                now,
                style,
            ));
            out.push_str(&render_window(
                "weekly",
                report.usage.seven_day.as_ref(),
                now,
                style,
            ));
        }
        Err(error) => {
            out.push_str(&header);
            out.push('\n');
            out.push_str(&format!(
                "    {}\n",
                style.paint("31", &format!("{error:#}"))
            ));
        }
    }
    out
}

fn render_window(label: &str, window: Option<&Window>, now: u64, style: &Style) -> String {
    let Some(window) = window else {
        return format!(
            "    {label:<6}  {}\n",
            style.paint("2", "no limit reported")
        );
    };
    // Show what is left, so a full green bar means the whole limit is available.
    let left = 100.0 - window.utilization.clamp(0.0, 100.0);
    let filled = ((left / 100.0) * BAR_WIDTH as f64).round() as usize;
    let color = match left {
        l if l <= 20.0 => "31",
        l if l <= 50.0 => "33",
        _ => "32",
    };
    let bar = format!(
        "{}{}",
        style.paint(color, &"█".repeat(filled)),
        style.paint("2", &"░".repeat(BAR_WIDTH - filled))
    );
    let reset = window
        .resets_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|reset| {
            let remaining = reset.timestamp().saturating_sub(now as i64).max(0) as u64;
            format!(
                "  {}",
                style.paint("2", &format!("resets in {}", format_duration(remaining)))
            )
        })
        .unwrap_or_default();
    format!(
        "    {label:<6}  {bar}  {:>3}% left{reset}\n",
        left.round() as u64
    )
}

fn format_duration(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    match (days, hours) {
        (0, 0) => format!("{minutes}m"),
        (0, _) => format!("{hours}h {minutes:02}m"),
        _ => format!("{days}d {hours}h"),
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: Style = Style { color: false };

    #[test]
    fn keychain_service_matches_claude_code_scoping() {
        // sha256("abc") = ba7816bf8f01cfea...
        assert_eq!(
            keychain_service(Path::new("abc")),
            "Claude Code-credentials-ba7816bf"
        );
    }

    #[test]
    fn parses_subscription_credentials() {
        let token = parse_token(
            br#"{"claudeAiOauth":{"accessToken":"t","refreshToken":"r","expiresAt":1,"subscriptionType":"max"}}"#,
        )
        .unwrap();
        assert_eq!(token.access_token, "t");
        assert_eq!(token.expires_at, Some(1));
        assert_eq!(token.subscription_type.as_deref(), Some("max"));
    }

    #[test]
    fn rejects_credentials_without_a_subscription_login() {
        let error = parse_token(br#"{"mcpOAuth":{}}"#).unwrap_err();
        assert!(error.to_string().contains("not logged in"));
    }

    #[test]
    fn credentials_file_is_optional() {
        let temp = tempfile::tempdir().unwrap();
        assert!(read_credentials_file(temp.path()).unwrap().is_none());
        fs::write(temp.path().join(".credentials.json"), b"{}").unwrap();
        assert!(read_credentials_file(temp.path()).unwrap().is_some());
    }

    #[test]
    fn renders_bars_percentages_and_resets() {
        let usage: Usage = serde_json::from_str(
            r#"{"five_hour":{"utilization":35.4,"resets_at":"2026-01-01T02:14:00+00:00"},
                "seven_day":{"utilization":100.0,"resets_at":"2026-01-05T03:00:00Z"},
                "seven_day_opus":null}"#,
        )
        .unwrap();
        let now = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .timestamp() as u64;
        let report = Ok(Report {
            plan: Some("max".to_owned()),
            usage,
        });

        let rendered = render_profile("work", true, 8, &report, now, &PLAIN);

        assert_eq!(
            rendered,
            "* work      max\n\
             \x20   5h      █████████████░░░░░░░   65% left  resets in 2h 14m\n\
             \x20   weekly  ░░░░░░░░░░░░░░░░░░░░    0% left  resets in 4d 3h\n"
        );
    }

    #[test]
    fn renders_errors_without_hiding_other_profiles() {
        let report = Err(anyhow::anyhow!("login expired"));
        let rendered = render_profile("personal", false, 8, &report, 0, &PLAIN);
        assert_eq!(rendered, "  personal\n    login expired\n");
    }

    fn usage(five_hour_used: f64, weekly_used: f64) -> Usage {
        serde_json::from_value(serde_json::json!({
            "five_hour": {"utilization": five_hour_used, "resets_at": null},
            "seven_day": {"utilization": weekly_used, "resets_at": null}
        }))
        .unwrap()
    }

    #[test]
    fn weekly_limit_caps_available_usage() {
        assert_eq!(available(&usage(89.0, 12.0)), 11.0);
        assert_eq!(available(&usage(0.0, 100.0)), 0.0);
        assert_eq!(available(&usage(20.0, 60.0)), 40.0);
    }

    #[test]
    fn picks_the_profile_with_most_available_usage() {
        assert_eq!(
            pick_best(&[("rtp", 11.0), ("skk", 0.0), ("team", 40.0)], Some("rtp")),
            Some("team")
        );
    }

    #[test]
    fn ties_prefer_the_active_profile_then_the_first_name() {
        let tied = [("a", 50.0), ("b", 50.0), ("c", 50.0)];
        assert_eq!(pick_best(&tied, Some("b")), Some("b"));
        assert_eq!(pick_best(&tied, None), Some("a"));
    }

    #[test]
    fn exhausted_profiles_are_never_picked() {
        assert_eq!(pick_best(&[("rtp", 0.0), ("skk", 0.0)], None), None);
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(59), "0m");
        assert_eq!(format_duration(3_660), "1h 01m");
        assert_eq!(format_duration(90_000), "1d 1h");
    }
}
