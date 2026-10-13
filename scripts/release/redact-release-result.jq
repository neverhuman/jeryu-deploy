# Redact both the summary and tail before sending host output to the forge.
def redact:
  gsub("jpat_[0-9a-fA-F]{64}"; "[REDACTED]")
  | gsub("(?i)(bearer|token|password|secret|api[_-]?key)[=: ]+[^ \t\r\n&]+"; "[REDACTED]")
  | gsub("https?://[^ /@\t\r\n]+:[^ /@\t\r\n]+@"; "https://[REDACTED]@")
  | gsub("https?://[^ \t\r\n?]+\\?[^ \t\r\n]+"; "[URL QUERY REDACTED]");
{ok: $ok, message: ($message | redact), log_tail: ($log[-8000:] | redact)}
