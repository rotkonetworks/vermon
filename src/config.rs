use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub refresh_interval: Duration,
    pub github_token: Option<String>,
    pub port: u16,
    pub max_retries: u32,
    pub cert_warning_days: i64,
    pub cert_critical_days: i64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            refresh_interval: Duration::from_secs(3600),
            github_token: std::env::var("GITHUB_TOKEN").ok(),
            port: std::env::var("PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(3000),
            max_retries: 3,
            cert_warning_days: std::env::var("CERT_WARNING_DAYS")
                .ok()
                .and_then(|d| d.parse().ok())
                .unwrap_or(30),
            cert_critical_days: std::env::var("CERT_CRITICAL_DAYS")
                .ok()
                .and_then(|d| d.parse().ok())
                .unwrap_or(7),
        }
    }
}

//────────────────── Repository configuration
#[derive(Debug, Clone)]
pub struct RepoConfig {
    pub owner: &'static str,
    pub name: &'static str,
    pub prefix: &'static str,
    pub networks: &'static [&'static str],
    pub version_pattern: Option<&'static str>,
}
