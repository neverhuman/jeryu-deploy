//! Server-side Markdown rendering for READMEs and `.md` blobs.
//!
//! CommonMark via `pulldown-cmark` plus the GitHub extensions people expect in
//! a README (tables, strikethrough, task lists, footnotes). The output is safe
//! on its own, before the web UI's DOMPurify pass: raw HTML in the source is
//! emitted as escaped text, and links/images with a script-capable scheme are
//! neutralised to `#`.

use std::collections::HashMap;

use jeryu_readmodel::contracts::{MarkdownHeading, MarkdownLink, RenderedMarkdown};
use pulldown_cmark::{CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd, html};

/// Where a Markdown file lives, so its relative URLs can be resolved the way
/// a repository host does: images to the path-form raw file beside it, links
/// to the web app's file or folder view.
pub(super) struct MarkdownBase<'a> {
    /// The repository id the raw route takes.
    pub(super) repo_id: &'a str,
    /// The repository's web-app root, `/repos/<host>/<owner>/<name>`.
    pub(super) web_root: &'a str,
    pub(super) ref_name: &'a str,
    /// The Markdown file's directory, `""` at the root, no trailing `/`.
    pub(super) dir: &'a str,
    /// Whether a repository path is a directory (a link then opens the tree
    /// view instead of the blob view).
    pub(super) is_dir: &'a dyn Fn(&str) -> bool,
}

pub(super) fn render_markdown(markdown: &str) -> RenderedMarkdown {
    render_markdown_in(markdown, None)
}

/// [`render_markdown`], with relative image and link URLs resolved against
/// `base` when one is given.
pub(super) fn render_markdown_in(
    markdown: &str,
    base: Option<&MarkdownBase<'_>>,
) -> RenderedMarkdown {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let mut events: Vec<Event<'_>> = Parser::new_ext(markdown, options)
        .map(|event| match event {
            // Never pass author HTML through; show it as text instead.
            Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
            other => other,
        })
        .collect();

    let mut toc = Vec::new();
    let mut links = Vec::new();
    let mut used_ids: HashMap<String, usize> = HashMap::new();
    for index in 0..events.len() {
        if let Event::Start(Tag::Heading { level, .. }) = &events[index] {
            let depth = heading_depth(*level);
            let text = heading_text(&events[index + 1..]);
            let slug = unique_slug(&text, &mut used_ids);
            if let Event::Start(Tag::Heading { id, .. }) = &mut events[index] {
                *id = Some(CowStr::from(slug.clone()));
            }
            toc.push(MarkdownHeading {
                depth,
                id: slug,
                text,
            });
            continue;
        }
        match &mut events[index] {
            Event::Start(Tag::Link { dest_url, .. }) => {
                if is_unsafe_url(dest_url) {
                    *dest_url = CowStr::Borrowed("#");
                } else {
                    if let Some(base) = base
                        && let Some(resolved) = relative_target(dest_url, base, Target::Link)
                    {
                        *dest_url = CowStr::from(resolved);
                    }
                    links.push(markdown_link(dest_url));
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                if is_unsafe_url(dest_url) {
                    *dest_url = CowStr::Borrowed("#");
                } else if let Some(base) = base
                    && let Some(resolved) = relative_target(dest_url, base, Target::Image)
                {
                    *dest_url = CowStr::from(resolved);
                }
            }
            _ => {}
        }
    }

    let mut rendered = String::with_capacity(markdown.len() * 3 / 2);
    html::push_html(&mut rendered, events.into_iter());
    RenderedMarkdown {
        html: rendered,
        toc,
        links,
        renderer_version: "jeryu-md-renderer.v2".to_string(),
        sanitizer_version: Some("jeryu-md-sanitizer.v2".to_string()),
        rendered_at: super::server_time(),
    }
}

fn heading_depth(level: HeadingLevel) -> u32 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Plain text of the heading whose start tag precedes `rest`.
fn heading_text(rest: &[Event<'_>]) -> String {
    let mut text = String::new();
    for event in rest {
        match event {
            Event::End(TagEnd::Heading(_)) => break,
            Event::Text(value) | Event::Code(value) => text.push_str(value),
            _ => {}
        }
    }
    text.trim().to_string()
}

fn unique_slug(text: &str, used: &mut HashMap<String, usize>) -> String {
    let base = slug(text);
    let count = used.entry(base.clone()).or_insert(0);
    let id = if *count == 0 {
        base.clone()
    } else {
        format!("{base}-{count}")
    };
    *count += 1;
    id
}

fn slug(value: &str) -> String {
    let slug = value
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "section".to_string()
    } else {
        slug
    }
}

fn is_unsafe_url(url: &str) -> bool {
    let scheme: String = url
        .trim_start()
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace() && !ch.is_ascii_control())
        .take_while(|ch| *ch != ':' && *ch != '/' && *ch != '?' && *ch != '#')
        .collect::<String>()
        .to_ascii_lowercase();
    url.contains(':') && matches!(scheme.as_str(), "javascript" | "vbscript" | "data")
}

#[derive(Clone, Copy, PartialEq)]
enum Target {
    Image,
    Link,
}

/// The rewritten URL for a relative `url`, or `None` to leave it as written.
///
/// Relative means: no scheme, not protocol-relative, no leading `/`, and not
/// a bare `#fragment`. The path is percent-decoded, joined to the file's
/// directory, and `.`/`..` are folded; a path that climbs above the
/// repository root becomes `#`. Images go to the path-form raw URL; links go
/// to the web app's blob view, or its tree view for a directory, keeping any
/// `#fragment`.
fn relative_target(url: &str, base: &MarkdownBase<'_>, target: Target) -> Option<String> {
    let url = url.trim();
    if url.is_empty() || url.starts_with(['/', '#', '?']) || has_scheme(url) {
        return None;
    }
    let (rest, fragment) = match url.split_once('#') {
        Some((rest, fragment)) => (rest, Some(fragment)),
        None => (url, None),
    };
    // A query means nothing to a file in a repository.
    let path_part = rest.split_once('?').map_or(rest, |(path, _)| path);
    let trailing_slash = path_part.ends_with('/');
    let mut parts: Vec<String> = base
        .dir
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect();
    for raw in path_part.split('/') {
        let part = super::auth::percent_decode(raw);
        match part.as_str() {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Some("#".to_string());
                }
            }
            _ if part.contains('\0') => return Some("#".to_string()),
            _ => parts.push(part),
        }
    }
    let path = parts
        .iter()
        .map(|part| encode_segment(part))
        .collect::<Vec<_>>()
        .join("/");
    let resolved = match target {
        Target::Image => {
            if path.is_empty() {
                return Some("#".to_string());
            }
            let ref_path = base
                .ref_name
                .split('/')
                .map(encode_segment)
                .collect::<Vec<_>>()
                .join("/");
            format!(
                "/api/v1/repos/{}/raw/{ref_path}/{path}",
                encode_segment(base.repo_id)
            )
        }
        Target::Link => {
            let joined = parts.join("/");
            let view = if trailing_slash || joined.is_empty() || (base.is_dir)(&joined) {
                "tree"
            } else {
                "blob"
            };
            let mut link = format!("{}/{view}/{}", base.web_root, encode_segment(base.ref_name));
            if !path.is_empty() {
                link.push('/');
                link.push_str(&path);
            }
            if let Some(fragment) = fragment {
                link.push('#');
                link.push_str(fragment);
            }
            link
        }
    };
    Some(resolved)
}

/// `scheme:` per RFC 3986, or a protocol-relative `//host`.
fn has_scheme(url: &str) -> bool {
    if url.starts_with("//") {
        return true;
    }
    let Some((scheme, _)) = url.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Percent-encode one path segment (everything but RFC 3986 unreserved).
pub(super) fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn markdown_link(href: &str) -> MarkdownLink {
    let external = href.starts_with("http://")
        || href.starts_with("https://")
        || href.starts_with("mailto:")
        || href.starts_with("//");
    MarkdownLink {
        href: href.to_string(),
        resolved_route: None,
        external,
    }
}

#[cfg(test)]
mod tests {
    use super::{MarkdownBase, render_markdown, render_markdown_in};

    fn rendered_in(markdown: &str, dir: &str) -> String {
        let is_dir = |path: &str| path == "showcase/set-a" || path == "docs";
        let base = MarkdownBase {
            repo_id: "repo-1",
            web_root: "/repos/jeryu/acme/widgets",
            ref_name: "release/v1",
            dir,
            is_dir: &is_dir,
        };
        render_markdown_in(markdown, Some(&base)).html
    }

    #[test]
    fn relative_images_go_to_the_raw_file_beside_the_document() {
        let html = rendered_in(
            "![a](shot.webp) ![b](./nested/deep/b.png) ![c](../other/c.png) ![d](img%20one.png) ![e](x.png?raw=1#frag)",
            "showcase/set-a",
        );
        let raw = "/api/v1/repos/repo-1/raw/release/v1";
        assert!(
            html.contains(&format!("src=\"{raw}/showcase/set-a/shot.webp\"")),
            "{html}"
        );
        assert!(
            html.contains(&format!("src=\"{raw}/showcase/set-a/nested/deep/b.png\"")),
            "{html}"
        );
        assert!(
            html.contains(&format!("src=\"{raw}/showcase/other/c.png\"")),
            "{html}"
        );
        assert!(
            html.contains(&format!("src=\"{raw}/showcase/set-a/img%20one.png\"")),
            "{html}"
        );
        assert!(
            html.contains(&format!("src=\"{raw}/showcase/set-a/x.png\"")),
            "{html}"
        );
    }

    #[test]
    fn relative_links_open_the_web_view_and_escapes_are_refused() {
        let html = rendered_in(
            "[guide](../guide.md#setup) [set](./) [docs](../../docs) [up](../../../etc/passwd) ![up](../../../x.png) [top](#top)",
            "showcase/set-a",
        );
        let web = "/repos/jeryu/acme/widgets";
        assert!(
            html.contains(&format!(
                "href=\"{web}/blob/release%2Fv1/showcase/guide.md#setup\""
            )),
            "{html}"
        );
        assert!(
            html.contains(&format!("href=\"{web}/tree/release%2Fv1/showcase/set-a\"")),
            "{html}"
        );
        assert!(
            html.contains(&format!("href=\"{web}/tree/release%2Fv1/docs\"")),
            "{html}"
        );
        assert!(html.contains("href=\"#\">up</a>"), "{html}");
        assert!(html.contains("src=\"#\" alt=\"up\""), "{html}");
        assert!(html.contains("href=\"#top\""), "{html}");
        assert!(!html.contains("passwd"), "{html}");
    }

    #[test]
    fn absolute_external_and_unsafe_urls_are_not_rebased() {
        let html = rendered_in(
            "![abs](/static/x.png) ![ext](https://cdn.example/x.png) ![proto](//cdn.example/y.png) [mail](mailto:a@example.com) ![inline](data:image/png;base64,AAAA) [js](javascript:alert(1))",
            "docs",
        );
        assert!(html.contains("src=\"/static/x.png\""), "{html}");
        assert!(html.contains("src=\"https://cdn.example/x.png\""), "{html}");
        assert!(html.contains("src=\"//cdn.example/y.png\""), "{html}");
        assert!(html.contains("href=\"mailto:a@example.com\""), "{html}");
        // data: and javascript: stay neutralised exactly as without a base.
        assert!(!html.contains("data:image"), "{html}");
        assert!(!html.contains("javascript:"), "{html}");
        assert!(!html.contains("/api/v1/repos/"), "{html}");
    }

    #[test]
    fn without_a_base_relative_urls_are_left_as_written() {
        let out = render_markdown("![a](shot.webp) [b](other.md)");
        assert!(out.html.contains("src=\"shot.webp\""));
        assert!(out.html.contains("href=\"other.md\""));
    }

    #[test]
    fn renders_commonmark_blocks_not_one_paragraph_per_line() {
        let out = render_markdown(
            "# Jeryu\n\n**Bold** intro that\nwraps.\n\n## Highlights\n\n- one\n- two\n\n```bash\n# not a heading\necho hi\n```\n",
        );
        assert!(out.html.contains("<h1 id=\"jeryu\">Jeryu</h1>"));
        assert!(
            out.html
                .contains("<p><strong>Bold</strong> intro that\nwraps.</p>")
        );
        assert!(out.html.contains("<h2 id=\"highlights\">Highlights</h2>"));
        assert!(out.html.contains("<ul>\n<li>one</li>\n<li>two</li>\n</ul>"));
        assert!(out.html.contains(
            "<pre><code class=\"language-bash\"># not a heading\necho hi\n</code></pre>"
        ));
        let toc: Vec<_> = out.toc.iter().map(|h| (h.depth, h.id.as_str())).collect();
        assert_eq!(toc, vec![(1, "jeryu"), (2, "highlights")]);
    }

    #[test]
    fn escapes_raw_html_and_neutralises_script_links() {
        let out = render_markdown(
            "<script>alert(1)</script>\n\n[x](javascript:alert(1)) [y]( JaVaScRiPt:alert(1)) ![i](data:text/html,hi) [ok](https://example.com)\n",
        );
        assert!(!out.html.contains("<script>"));
        assert!(out.html.contains("&lt;script&gt;"));
        assert!(!out.html.to_ascii_lowercase().contains("javascript:"));
        assert!(!out.html.contains("data:text/html"));
        assert!(out.html.contains("href=\"https://example.com\""));
        assert_eq!(out.links.len(), 1);
        assert!(out.links[0].external);
    }

    #[test]
    fn deduplicates_heading_ids_and_renders_tables() {
        let out = render_markdown("## Usage\n\n## Usage\n\n| a | b |\n|---|---|\n| 1 | 2 |\n");
        let ids: Vec<_> = out.toc.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, vec!["usage", "usage-1"]);
        assert!(out.html.contains("<table>"));
    }
}
