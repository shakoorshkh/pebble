/// Combine stdout and stderr into a single lowercase string for pattern matching.
///
/// Lowercasing here means every classifier can use plain lowercase literals
/// without calling `.to_lowercase()` themselves — one allocation per classify call.
pub(crate) fn combined_output(stdout: &str, stderr: &str) -> String {
    let mut out = String::with_capacity(stdout.len() + stderr.len() + 1);
    out.push_str(stdout);
    out.push('\n');
    out.push_str(stderr);
    out.to_lowercase()
}

/// Return true if `text` contains any of the given needle strings.
pub(crate) fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}
