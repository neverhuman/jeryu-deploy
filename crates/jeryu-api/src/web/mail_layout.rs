//! The branded layout for a message a person reads: a card with the JeRyū
//! wordmark, a heading, a few paragraphs, at most one button, quieter notes,
//! and a footer that says why the message came and where a reply goes.
//!
//! Mail clients ignore stylesheets and most of CSS, so this is tables and
//! inline styles only, 560px wide, with the system font stack. The wordmark
//! is a PNG served by the web app (Gmail and Outlook drop SVG); with images
//! off, its alt text is styled to stand in. A `prefers-color-scheme` block
//! repaints the card for clients that honour it, matched on the inline colours
//! below, so a colour changed here must change there too.
//!
//! Every value is escaped before it reaches the HTML. The text rendering
//! carries the same words and every link written out.

use super::mail::MailBrand;

const SANS: &str = "-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif";
const MONO: &str = "ui-monospace,SFMono-Regular,Menlo,Consolas,'Liberation Mono',monospace";
const PAGE: &str = "#F2F6FC";
const CARD: &str = "#FFFFFF";
const LINE: &str = "#C6D5E2";
const TEXT: &str = "#15243B";
const DIM: &str = "#536780";
const INK: &str = "#07111F";
const STRIPE: &str = "#4CC3EF";
const BUTTON: &str = "#7DE3F5";
const BUTTON_TEXT: &str = "#061522";
const LOGO_INK: &str = "#EEF4FC";
/// Where the web app serves the wordmark, from the public origin.
pub(crate) const WORDMARK_PATH: &str = "email/jeryu-wordmark.png";

/// One message's words. The layout adds the frame and the footer.
pub(crate) struct BrandedMessage<'a> {
    /// The `<title>`; the subject line itself is the caller's.
    pub title: &'a str,
    /// The line a mail client previews beside the subject.
    pub preheader: &'a str,
    pub heading: &'a str,
    pub paragraphs: &'a [&'a str],
    /// `(label, url)`; the URL is also written out under the button.
    pub button: Option<(&'a str, &'a str)>,
    /// Smaller, quieter text after the button.
    pub notes: &'a [&'a str],
    /// Why this address got the message, first in the footer.
    pub why: &'a str,
}

impl BrandedMessage<'_> {
    /// The HTML body. `origin` is the public base URL, without a trailing `/`.
    pub(crate) fn render_html(&self, origin: &str, brand: &MailBrand) -> String {
        let mut html = String::new();
        html.push_str(&head(self.title));
        html.push_str(&format!(
            "<body style=\"margin:0;padding:0;background-color:{PAGE};-webkit-text-size-adjust:100%;\">\n"
        ));
        html.push_str(&format!(
            "<div style=\"display:none;font-size:1px;line-height:1px;max-height:0;max-width:0;opacity:0;overflow:hidden;mso-hide:all;\">{}{}</div>\n",
            escape(self.preheader),
            "&zwnj;&nbsp;".repeat(30)
        ));
        html.push_str(&format!(
            "<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" style=\"background-color:{PAGE};\"><tr><td align=\"center\" style=\"padding:24px 12px;\">\n"
        ));
        html.push_str(&format!(
            "<table role=\"presentation\" width=\"560\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" style=\"width:100%;max-width:560px;background-color:{CARD};border:1px solid {LINE};border-radius:10px;\">\n"
        ));
        html.push_str(&format!(
            "<tr><td bgcolor=\"{INK}\" align=\"left\" style=\"background-color:{INK};padding:20px 28px;border-radius:10px 10px 0 0;\"><img src=\"{}\" width=\"124\" height=\"38\" alt=\"JeRyū\" style=\"display:block;border:0;outline:none;text-decoration:none;width:124px;height:38px;color:{LOGO_INK};font-family:{SANS};font-size:22px;font-weight:700;font-style:italic;line-height:38px;\"></td></tr>\n",
            escape(&format!("{origin}/{WORDMARK_PATH}"))
        ));
        html.push_str(&format!(
            "<tr><td bgcolor=\"{STRIPE}\" style=\"background-color:{STRIPE};height:3px;line-height:3px;font-size:0;\">&nbsp;</td></tr>\n"
        ));
        html.push_str("<tr><td style=\"padding:30px 28px 8px 28px;\">\n");
        html.push_str(&format!(
            "<h1 style=\"margin:0 0 12px 0;font-family:{SANS};font-size:21px;line-height:28px;font-weight:700;color:{TEXT};\">{}</h1>\n",
            escape(self.heading)
        ));
        for paragraph in self.paragraphs {
            html.push_str(&paragraph_html(paragraph, TEXT));
        }
        html.push_str("</td></tr>\n");
        if let Some((label, url)) = self.button {
            html.push_str("<tr><td style=\"padding:8px 28px 6px 28px;\">\n");
            html.push_str(&format!(
                "<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\"><tr><td bgcolor=\"{BUTTON}\" align=\"center\" style=\"background-color:{BUTTON};border-radius:999px;\"><a href=\"{url}\" style=\"display:inline-block;padding:13px 26px;font-family:{SANS};font-size:15px;font-weight:600;line-height:18px;color:{BUTTON_TEXT};text-decoration:none;border-radius:999px;\">{label}</a></td></tr></table>\n",
                url = escape(url),
                label = escape(label)
            ));
            html.push_str(&format!(
                "<p style=\"margin:12px 0 10px 0;font-family:{MONO};font-size:12px;line-height:18px;color:{DIM};word-break:break-all;\">{}</p>\n",
                escape(url)
            ));
            html.push_str("</td></tr>\n");
        }
        if !self.notes.is_empty() {
            html.push_str("<tr><td style=\"padding:4px 28px 8px 28px;\">\n");
            for note in self.notes {
                html.push_str(&paragraph_html(note, DIM));
            }
            html.push_str("</td></tr>\n");
        }
        html.push_str(&format!(
            "<tr><td style=\"padding:6px 28px 24px 28px;\"><div style=\"border-top:1px solid {LINE};height:1px;line-height:1px;font-size:0;margin-bottom:16px;\">&nbsp;</div>\n"
        ));
        html.push_str(&footer_html(self.why));
        if let Some(reply_to) = &brand.reply_to {
            html.push_str(&footer_html_raw(&format!(
                "Questions? Reply to this email and a person will answer, or write to <a href=\"mailto:{address}\" style=\"color:{TEXT};text-decoration:underline;\">{address}</a>.",
                address = escape(reply_to)
            )));
        }
        if let Some(footer) = &brand.footer {
            let line = match &brand.footer_url {
                Some(url) => format!(
                    "<a href=\"{}\" style=\"color:{TEXT};text-decoration:underline;\">{}</a>",
                    escape(url),
                    escape(footer)
                ),
                None => escape(footer),
            };
            html.push_str(&footer_html_raw(&line));
        }
        html.push_str("</td></tr>\n</table>\n</td></tr></table>\n</body>\n</html>\n");
        html
    }

    /// The plain-text body: the same words, every link written out.
    pub(crate) fn render_text(&self, brand: &MailBrand) -> String {
        let mut text = String::new();
        text.push_str(self.heading);
        text.push_str("\n\n");
        for paragraph in self.paragraphs {
            text.push_str(paragraph);
            text.push_str("\n\n");
        }
        if let Some((label, url)) = self.button {
            text.push_str(&format!("{label}: {url}\n\n"));
        }
        for note in self.notes {
            text.push_str(note);
            text.push_str("\n\n");
        }
        text.push_str("--\n");
        text.push_str(self.why);
        text.push('\n');
        if let Some(reply_to) = &brand.reply_to {
            text.push_str(&format!(
                "Questions? Reply to this email and a person will answer, or write to {reply_to}.\n"
            ));
        }
        if let Some(footer) = &brand.footer {
            match &brand.footer_url {
                Some(url) => text.push_str(&format!("{footer} ({url})\n")),
                None => text.push_str(&format!("{footer}\n")),
            }
        }
        text
    }
}

fn head(title: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n\
         <meta name=\"color-scheme\" content=\"light dark\">\n\
         <meta name=\"supported-color-schemes\" content=\"light dark\">\n\
         <title>{}</title>\n<style>\n\
         @media (prefers-color-scheme: dark) {{\n\
         body, table[style*=\"background-color:{PAGE}\"] {{ background-color:#07111F !important; }}\n\
         table[style*=\"background-color:{CARD}\"] {{ background-color:#0E1B2C !important; }}\n\
         [style*=\"border:1px solid {LINE}\"] {{ border-color:#263F55 !important; }}\n\
         [style*=\"border-top:1px solid {LINE}\"] {{ border-top-color:#263F55 !important; }}\n\
         td[bgcolor=\"{INK}\"][align=\"left\"] {{ background-color:#15283A !important; }}\n\
         [style*=\"color:{TEXT}\"] {{ color:#EEF4FC !important; }}\n\
         [style*=\"color:{DIM}\"] {{ color:#A5BACE !important; }}\n\
         }}\n</style>\n</head>\n",
        escape(title)
    )
}

fn paragraph_html(words: &str, color: &str) -> String {
    format!(
        "<p style=\"margin:0 0 14px 0;font-family:{SANS};font-size:15px;line-height:23px;color:{color};\">{}</p>\n",
        escape(words)
    )
}

fn footer_html(words: &str) -> String {
    footer_html_raw(&escape(words))
}

/// A footer line whose markup the caller built and escaped.
fn footer_html_raw(markup: &str) -> String {
    format!(
        "<p style=\"margin:0 0 8px 0;font-family:{SANS};font-size:12px;line-height:18px;color:{DIM};\">{markup}</p>\n"
    )
}

/// Escape text for an HTML body or a double-quoted attribute.
pub(crate) fn escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message<'a>(paragraphs: &'a [&'a str], notes: &'a [&'a str]) -> BrandedMessage<'a> {
        BrandedMessage {
            title: "Hello",
            preheader: "A short preview",
            heading: "You're in <b>",
            paragraphs,
            button: Some(("Open", "https://forge.example/?a=1&b=2")),
            notes,
            why: "You asked for this.",
        }
    }

    #[test]
    fn every_value_is_escaped() {
        let html = message(&["a < b & \"c\""], &["it's"])
            .render_html("https://forge.example", &MailBrand::default());
        assert!(html.contains("You&#39;re in &lt;b&gt;"), "{html}");
        assert!(html.contains("a &lt; b &amp; &quot;c&quot;"), "{html}");
        assert!(
            html.contains("href=\"https://forge.example/?a=1&amp;b=2\""),
            "{html}"
        );
        assert!(!html.contains("<b>"), "{html}");
    }

    #[test]
    fn the_wordmark_is_served_from_the_public_origin() {
        let html = message(&[], &[]).render_html("https://forge.example", &MailBrand::default());
        assert!(
            html.contains("src=\"https://forge.example/email/jeryu-wordmark.png\""),
            "{html}"
        );
        assert!(html.contains("alt=\"JeRyū\""), "{html}");
    }

    #[test]
    fn the_footer_carries_only_what_is_configured() {
        let bare = message(&[], &[]);
        let html = bare.render_html("https://forge.example", &MailBrand::default());
        assert!(!html.contains("Questions?"), "{html}");
        assert!(!html.contains("mailto:"), "{html}");

        let brand = MailBrand {
            reply_to: Some("help@example.com".to_string()),
            footer: Some("Run by Example & Co".to_string()),
            footer_url: Some("https://example.com".to_string()),
        };
        let html = bare.render_html("https://forge.example", &brand);
        assert!(html.contains("href=\"mailto:help@example.com\""), "{html}");
        assert!(
            html.contains("<a href=\"https://example.com\" style=\"color:#15243B;text-decoration:underline;\">Run by Example &amp; Co</a>"),
            "{html}"
        );
        let text = bare.render_text(&brand);
        assert!(text.contains("write to help@example.com."), "{text}");
        assert!(
            text.contains("Run by Example & Co (https://example.com)"),
            "{text}"
        );
    }

    #[test]
    fn the_text_carries_the_words_and_the_link() {
        let text = message(&["First."], &["Quiet."]).render_text(&MailBrand::default());
        assert!(text.starts_with("You're in <b>\n\nFirst.\n\n"), "{text}");
        assert!(
            text.contains("Open: https://forge.example/?a=1&b=2"),
            "{text}"
        );
        assert!(text.contains("Quiet."), "{text}");
        assert!(text.contains("--\nYou asked for this.\n"), "{text}");
    }
}
