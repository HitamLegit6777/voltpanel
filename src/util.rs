//! Small utilities shared across the app.
use anyhow::Result;

/// Format bytes to human readable (KB/MB/GB).
pub fn human_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Safe file-name sanitization.
pub fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect()
}

/// Parse "1d2h3m" style duration to seconds (used by some settings).
pub fn parse_duration(s: &str) -> Result<u64> {
    let mut total: u64 = 0;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let n: u64 = num.parse()?;
            num.clear();
            total += match c {
                'd' => n * 86400,
                'h' => n * 3600,
                'm' => n * 60,
                's' => n,
                _ => return Err(anyhow::anyhow!("invalid duration char {c}")),
            };
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.00 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.00 MB");
    }

    #[test]
    fn duration_parses() {
        assert_eq!(parse_duration("1h30m").unwrap(), 5400);
        assert_eq!(parse_duration("2d").unwrap(), 172800);
        assert!(parse_duration("1x").is_err());
    }

    #[test]
    fn sanitize_drops_bad() {
        assert_eq!(sanitize_name("a/b\\c"), "a_b_c");
    }
}
