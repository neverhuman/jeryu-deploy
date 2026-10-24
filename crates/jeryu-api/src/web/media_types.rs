//! The one extension -> media type table, shared by the SPA's static files
//! and by repository raw/blob reads.
//!
//! The table says what a file is. Whether the forge lets a browser *run* it
//! is a separate decision made by each caller: the SPA serves its own
//! JavaScript as JavaScript, while repository content never is (see
//! `repositories::source::mime_for_repo_file`).

/// The media type for a file extension (any case), or `None` when the table
/// does not know it.
pub(super) fn by_extension(extension: &str) -> Option<&'static str> {
    Some(match extension.to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "md" | "markdown" => "text/markdown; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        _ => return None,
    })
}

/// The extension of the last path segment, if it has one.
pub(super) fn extension_of(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next()?;
    name.rsplit_once('.')
        .map(|(_, ext)| ext)
        .filter(|ext| !ext.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_covers_the_media_a_page_loads() {
        for (ext, mime) in [
            ("png", "image/png"),
            ("JPG", "image/jpeg"),
            ("jpeg", "image/jpeg"),
            ("gif", "image/gif"),
            ("webp", "image/webp"),
            ("avif", "image/avif"),
            ("svg", "image/svg+xml"),
            ("ico", "image/x-icon"),
            ("pdf", "application/pdf"),
            ("mp4", "video/mp4"),
            ("webm", "video/webm"),
            ("woff", "font/woff"),
            ("woff2", "font/woff2"),
            ("wasm", "application/wasm"),
            ("js", "text/javascript; charset=utf-8"),
        ] {
            assert_eq!(by_extension(ext), Some(mime), "{ext}");
        }
        assert_eq!(by_extension("unknownext"), None);
        assert_eq!(extension_of("a/b.c/file"), None);
        assert_eq!(extension_of("a/shot.WEBP"), Some("WEBP"));
    }
}
