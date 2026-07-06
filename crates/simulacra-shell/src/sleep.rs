//! Bounded `sleep` compatibility builtin.

use std::time::Duration;

use crate::CommandResult;

const MAX_SLEEP: Duration = Duration::from_secs(30);

pub(crate) fn builtin_sleep(args: &[String]) -> CommandResult {
    if args.is_empty() {
        return CommandResult::error(1, "sleep: missing duration\n");
    }
    if args.len() > 1 {
        return CommandResult::error(1, "sleep: too many operands\n");
    }

    let raw = &args[0];
    let duration = match parse_duration(raw) {
        Ok(duration) => duration,
        Err(reason) => {
            return CommandResult::error(1, format!("sleep: invalid duration '{raw}': {reason}\n"));
        }
    };

    if duration > MAX_SLEEP {
        return CommandResult::error(
            1,
            format!("sleep: duration '{raw}' exceeds maximum supported duration of 30s\n"),
        );
    }

    std::thread::sleep(duration);
    CommandResult::success("")
}

fn parse_duration(raw: &str) -> Result<Duration, &'static str> {
    if raw.is_empty() || raw.starts_with('-') {
        return Err("expected a non-negative number of seconds");
    }

    if let Some(millis) = raw.strip_suffix("ms") {
        return parse_decimal_duration(millis, 1);
    }

    let seconds = raw.strip_suffix('s').unwrap_or(raw);
    parse_decimal_duration(seconds, 1_000)
}

fn parse_decimal_duration(raw: &str, millis_per_unit: u64) -> Result<Duration, &'static str> {
    if raw.is_empty() {
        return Err("expected a non-negative number");
    }

    let (whole, fraction) = raw.split_once('.').unwrap_or((raw, ""));
    if whole.is_empty() && fraction.is_empty() {
        return Err("expected a non-negative number");
    }
    if !whole.chars().all(|c| c.is_ascii_digit()) || !fraction.chars().all(|c| c.is_ascii_digit()) {
        return Err("expected a non-negative number");
    }

    let whole = if whole.is_empty() {
        0
    } else {
        whole.parse::<u64>().map_err(|_| "duration is too large")?
    };
    let whole_millis = whole
        .checked_mul(millis_per_unit)
        .ok_or("duration is too large")?;

    let fraction_millis = if fraction.is_empty() {
        0
    } else {
        scaled_fraction_to_millis(fraction, millis_per_unit)?
    };

    let total_millis = whole_millis
        .checked_add(fraction_millis)
        .ok_or("duration is too large")?;
    Ok(Duration::from_millis(total_millis))
}

fn scaled_fraction_to_millis(fraction: &str, millis_per_unit: u64) -> Result<u64, &'static str> {
    let mut scale = 1u64;
    let mut value = 0u64;
    for byte in fraction.bytes().take(3) {
        value = value
            .checked_mul(10)
            .and_then(|next| next.checked_add(u64::from(byte - b'0')))
            .ok_or("duration is too large")?;
        scale = scale.checked_mul(10).ok_or("duration is too large")?;
    }

    value
        .checked_mul(millis_per_unit)
        .map(|scaled| scaled / scale)
        .ok_or("duration is too large")
}
