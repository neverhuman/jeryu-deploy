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

pub(super) fn render_markdown(markdown: &str) -> RenderedMarkdown {
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
                    links.push(markdown_link(dest_url));
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) if is_unsafe_url(dest_url) => {
                *dest_url = CowStr::Borrowed("#");
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
    use super::render_markdown;

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
