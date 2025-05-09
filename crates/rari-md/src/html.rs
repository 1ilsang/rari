// Copyright (c) 2017–2024, Asherah Connor and Comrak contributors
// This code is part of Comrak and is licensed under the BSD 2-Clause License.
// See LICENSE file for more information.
// Modified by Florian Dieminger in 2024

//! The HTML renderer for the CommonMark AST, as well as helper functions.
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::str;

use comrak::adapters::HeadingMeta;
use comrak::nodes::{
    AstNode, ListType, NodeCode, NodeFootnoteDefinition, NodeMath, NodeTable, NodeValue,
    TableAlignment,
};
use comrak::{ComrakOptions, ComrakPlugins, Options, Plugins};
use itertools::Itertools;
use rari_types::locale::Locale;

use crate::anchor;
use crate::character_set::character_set;
use crate::ctype::isspace;
use crate::ext::{Flag, DELIM_START};
use crate::node_card::{alert_type_css_class, alert_type_default_title, is_callout, NoteCard};

/// Formats an AST as HTML, modified by the given options.
pub fn format_document<'a>(
    root: &'a AstNode<'a>,
    options: &ComrakOptions,
    output: &mut dyn Write,
    locale: Locale,
) -> io::Result<()> {
    format_document_with_plugins(root, options, output, &ComrakPlugins::default(), locale)
}

/// Formats an AST as HTML, modified by the given options. Accepts custom plugins.
pub fn format_document_with_plugins<'a>(
    root: &'a AstNode<'a>,
    options: &ComrakOptions,
    output: &mut dyn Write,
    plugins: &ComrakPlugins,
    locale: Locale,
) -> io::Result<()> {
    let mut writer = WriteWithLast {
        output,
        last_was_lf: Cell::new(true),
    };
    let mut f = HtmlFormatter::new(options, &mut writer, plugins);
    f.format(root, false, locale)?;
    if f.footnote_ix > 0 {
        f.output.write_all(b"</ol>\n</section>\n")?;
    }
    Ok(())
}

struct WriteWithLast<'w> {
    output: &'w mut dyn Write,
    last_was_lf: Cell<bool>,
}

impl Write for WriteWithLast<'_> {
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let l = buf.len();
        if l > 0 {
            self.last_was_lf.set(buf[l - 1] == 10);
        }
        self.output.write(buf)
    }
}

/// Converts header Strings to canonical, unique, but still human-readable, anchors.
///
/// To guarantee uniqueness, an anchorizer keeps track of the anchors
/// it has returned.  So, for example, to parse several MarkDown
/// files, use a new anchorizer per file.
///
/// ## Example
///
/// ```
/// use comrak::Anchorizer;
///
/// let mut anchorizer = Anchorizer::new();
///
/// // First "stuff" is unsuffixed.
/// assert_eq!("stuff".to_string(), anchorizer.anchorize("Stuff".to_string()));
/// // Second "stuff" has "-1" appended to make it unique.
/// assert_eq!("stuff-1".to_string(), anchorizer.anchorize("Stuff".to_string()));
/// ```
#[derive(Debug, Default)]
pub struct Anchorizer(HashSet<String>);

impl Anchorizer {
    /// Construct a new anchorizer.
    pub fn new() -> Self {
        Anchorizer(HashSet::new())
    }

    /// Returns a String that has been converted into an anchor using the
    /// GFM algorithm, which involves changing spaces to dashes, removing
    /// problem characters and, if needed, adding a suffix to make the
    /// resultant anchor unique.
    ///
    /// ```
    /// use comrak::Anchorizer;
    ///
    /// let mut anchorizer = Anchorizer::new();
    ///
    /// let source = "Ticks aren't in";
    ///
    /// assert_eq!("ticks-arent-in".to_string(), anchorizer.anchorize(source.to_string()));
    /// ```
    pub fn anchorize(&mut self, header: impl AsRef<str>) -> String {
        let id = anchor::anchorize(header.as_ref());

        let mut uniq = 0;
        let id = loop {
            let anchor = if uniq == 0 {
                Cow::from(id.as_ref())
            } else {
                Cow::from(format!("{}_{}", id, uniq + 1))
            };

            if !self.0.contains(&*anchor) {
                break anchor;
            }

            uniq += 1;
        };
        self.0.insert(id.to_string());
        id.to_string()
    }
}

struct HtmlFormatter<'o, 'c> {
    output: &'o mut WriteWithLast<'o>,
    options: &'o Options<'c>,
    anchorizer: Anchorizer,
    footnote_ix: u32,
    written_footnote_ix: u32,
    plugins: &'o ComrakPlugins<'o>,
}

fn tagfilter(literal: &[u8]) -> bool {
    static TAGFILTER_BLACKLIST: [&str; 9] = [
        "title",
        "textarea",
        "style",
        "xmp",
        "iframe",
        "noembed",
        "noframes",
        "script",
        "plaintext",
    ];

    if literal.len() < 3 || literal[0] != b'<' {
        return false;
    }

    let mut i = 1;
    if literal[i] == b'/' {
        i += 1;
    }

    let lc = unsafe { String::from_utf8_unchecked(literal[i..].to_vec()) }.to_lowercase();
    for t in TAGFILTER_BLACKLIST.iter() {
        if lc.starts_with(t) {
            let j = i + t.len();
            return isspace(literal[j])
                || literal[j] == b'>'
                || (literal[j] == b'/' && literal.len() >= j + 2 && literal[j + 1] == b'>');
        }
    }

    false
}

fn tagfilter_block(input: &[u8], o: &mut dyn Write) -> io::Result<()> {
    let size = input.len();
    let mut i = 0;

    while i < size {
        let org = i;
        while i < size && input[i] != b'<' {
            i += 1;
        }

        if i > org {
            o.write_all(&input[org..i])?;
        }

        if i >= size {
            break;
        }

        if tagfilter(&input[i..]) {
            o.write_all(b"&lt;")?;
        } else {
            o.write_all(b"<")?;
        }

        i += 1;
    }

    Ok(())
}

fn dangerous_url(_: &[u8]) -> bool {
    false
}

/// Writes buffer to output, escaping anything that could be interpreted as an
/// HTML tag.
///
/// Namely:
///
/// * U+0022 QUOTATION MARK " is rendered as &quot;
/// * U+0026 AMPERSAND & is rendered as &amp;
/// * U+003C LESS-THAN SIGN < is rendered as &lt;
/// * U+003E GREATER-THAN SIGN > is rendered as &gt;
/// * Everything else is passed through unchanged.
///
/// Note that this is appropriate and sufficient for free text, but not for
/// URLs in attributes.  See escape_href.
pub fn escape(output: &mut dyn Write, buffer: &[u8]) -> io::Result<()> {
    const HTML_UNSAFE: [bool; 256] = character_set!(b"&<>\"");

    let mut offset = 0;
    for (i, &byte) in buffer.iter().enumerate() {
        if HTML_UNSAFE[byte as usize] {
            let esc: &[u8] = match byte {
                b'"' => b"&quot;",
                b'&' => b"&amp;",
                b'<' => b"&lt;",
                b'>' => b"&gt;",
                _ => unreachable!(),
            };
            output.write_all(&buffer[offset..i])?;
            output.write_all(esc)?;
            offset = i + 1;
        }
    }
    output.write_all(&buffer[offset..])?;
    Ok(())
}

/// Writes buffer to output, escaping in a manner appropriate for URLs in HTML
/// attributes.
///
/// Namely:
///
/// * U+0026 AMPERSAND & is rendered as &amp;
/// * U+0027 APOSTROPHE ' is rendered as &#x27;
/// * Alphanumeric and a range of non-URL safe characters.
///
/// The inclusion of characters like "%" in those which are not escaped is
/// explained somewhat here:
///
/// https://github.com/github/cmark-gfm/blob/c32ef78bae851cb83b7ad52d0fbff880acdcd44a/src/houdini_href_e.c#L7-L31
///
/// In other words, if a CommonMark user enters:
///
/// ```markdown
/// [hi](https://ddg.gg/?q=a%20b)
/// ```
///
/// We assume they actually want the query string "?q=a%20b", a search for
/// the string "a b", rather than "?q=a%2520b", a search for the literal
/// string "a%20b".
pub fn escape_href(output: &mut dyn Write, buffer: &[u8]) -> io::Result<()> {
    let size = buffer.len();
    let mut i = 0;
    let mut escaped = "";

    while i < size {
        let org = i;
        while i < size {
            escaped = match buffer[i] {
                b'&' => "&amp;",
                b'<' => "&lt;",
                b'>' => "&gt;",
                b'"' => "&quot;",
                b'\'' => "&#x27;",
                _ => {
                    i += 1;
                    ""
                }
            };
            if !escaped.is_empty() {
                break;
            }
        }

        if i > org {
            output.write_all(&buffer[org..i])?;
        }

        if !escaped.is_empty() {
            output.write_all(escaped.as_bytes())?;
            escaped = "";
            i += 1;
        }
    }

    Ok(())
}

/// Writes an opening HTML tag, using an iterator to enumerate the attributes.
/// Note that attribute values are automatically escaped.
pub fn write_opening_tag<Str>(
    output: &mut dyn Write,
    tag: &str,
    attributes: impl IntoIterator<Item = (Str, Str)>,
) -> io::Result<()>
where
    Str: AsRef<str>,
{
    write!(output, "<{}", tag)?;
    for (attr, val) in attributes {
        write!(output, " {}=\"", attr.as_ref())?;
        escape(output, val.as_ref().as_bytes())?;
        output.write_all(b"\"")?;
    }
    output.write_all(b">")?;
    Ok(())
}

impl<'o, 'c> HtmlFormatter<'o, 'c>
where
    'c: 'o,
{
    fn new(
        options: &'o ComrakOptions<'c>,
        output: &'o mut WriteWithLast<'o>,
        plugins: &'o Plugins,
    ) -> Self {
        HtmlFormatter {
            options,
            output,
            anchorizer: Anchorizer::new(),
            footnote_ix: 0,
            written_footnote_ix: 0,
            plugins,
        }
    }

    fn cr(&mut self) -> io::Result<()> {
        if !self.output.last_was_lf.get() {
            self.output.write_all(b"\n")?;
        }
        Ok(())
    }

    fn escape(&mut self, buffer: &[u8]) -> io::Result<()> {
        escape(&mut self.output, buffer)
    }

    fn escape_href(&mut self, buffer: &[u8]) -> io::Result<()> {
        escape_href(&mut self.output, buffer)
    }

    fn format<'a>(&mut self, node: &'a AstNode<'a>, plain: bool, locale: Locale) -> io::Result<()> {
        // Traverse the AST iteratively using a work stack, with pre- and
        // post-child-traversal phases. During pre-order traversal render the
        // opening tags, then push the node back onto the stack for the
        // post-order traversal phase, then push the children in reverse order
        // onto the stack and begin rendering first child.

        enum Phase {
            Pre,
            Post,
        }
        let mut stack = vec![(node, plain, Phase::Pre, Flag::None)];

        while let Some((node, plain, phase, flag)) = stack.pop() {
            match phase {
                Phase::Pre => {
                    let new_plain = if plain {
                        match node.data.borrow().value {
                            NodeValue::Text(ref literal)
                            | NodeValue::Code(NodeCode { ref literal, .. })
                            | NodeValue::HtmlInline(ref literal) => {
                                self.escape(literal.as_bytes())?;
                            }
                            NodeValue::LineBreak | NodeValue::SoftBreak => {
                                self.output.write_all(b" ")?;
                            }
                            NodeValue::Math(NodeMath { ref literal, .. }) => {
                                self.escape(literal.as_bytes())?;
                            }
                            _ => (),
                        }
                        plain
                    } else {
                        let (new_plain, new_flag) = self.format_node(node, true, flag, locale)?;

                        stack.push((node, false, Phase::Post, new_flag));
                        new_plain
                    };

                    for ch in node.reverse_children() {
                        stack.push((ch, new_plain, Phase::Pre, Flag::None));
                    }
                }
                Phase::Post => {
                    debug_assert!(!plain);
                    self.format_node(node, false, flag, locale)?;
                }
            }
        }

        Ok(())
    }

    fn collect_text<'a>(node: &'a AstNode<'a>, output: &mut Vec<u8>) {
        match node.data.borrow().value {
            NodeValue::Text(ref literal) | NodeValue::Code(NodeCode { ref literal, .. }) => {
                output.extend_from_slice(literal.as_bytes())
            }
            NodeValue::LineBreak | NodeValue::SoftBreak => output.push(b' '),
            NodeValue::Math(NodeMath { ref literal, .. }) => {
                output.extend_from_slice(literal.as_bytes())
            }
            _ => {
                for n in node.children() {
                    Self::collect_text(n, output);
                }
            }
        }
    }

    fn format_node<'a>(
        &mut self,
        node: &'a AstNode<'a>,
        entering: bool,
        flag: Flag,
        locale: Locale,
    ) -> io::Result<(bool, Flag)> {
        match node.data.borrow().value {
            NodeValue::Document => (),
            NodeValue::FrontMatter(_) => (),
            NodeValue::BlockQuote => {
                self.cr()?;
                if entering {
                    let note_card = is_callout(node, locale);
                    match note_card {
                        Some(NoteCard::Callout) => {
                            self.output.write_all(b"<div class=\"callout\"")?;
                            self.render_sourcepos(node)?;
                            self.output.write_all(b">\n")?;
                            return Ok((false, Flag::Card));
                        }
                        Some(NoteCard::Note) => {
                            self.output
                                .write_all(b"<div class=\"notecard note\" data-add-note")?;
                            self.render_sourcepos(node)?;
                            self.output.write_all(b">\n")?;
                            return Ok((false, Flag::Card));
                        }
                        Some(NoteCard::Warning) => {
                            self.output
                                .write_all(b"<div class=\"notecard warning\" data-add-warning")?;
                            self.render_sourcepos(node)?;
                            self.output.write_all(b">\n")?;
                            return Ok((false, Flag::Card));
                        }
                        None => {
                            self.output.write_all(b"<blockquote")?;
                            self.render_sourcepos(node)?;
                            self.output.write_all(b">\n")?;
                        }
                    };
                } else if let Flag::Card = flag {
                    self.output.write_all(b"</div>\n")?;
                } else {
                    self.output.write_all(b"</blockquote>\n")?;
                }
            }
            NodeValue::List(ref nl) => {
                if entering {
                    self.cr()?;
                    match nl.list_type {
                        ListType::Bullet => {
                            self.output.write_all(b"<ul")?;
                            if nl.is_task_list && self.options.render.tasklist_classes {
                                self.output.write_all(b" class=\"contains-task-list\"")?;
                            }
                            self.render_sourcepos(node)?;
                            self.output.write_all(b">\n")?;
                        }
                        ListType::Ordered => {
                            self.output.write_all(b"<ol")?;
                            if nl.is_task_list && self.options.render.tasklist_classes {
                                self.output.write_all(b" class=\"contains-task-list\"")?;
                            }
                            self.render_sourcepos(node)?;
                            if nl.start == 1 {
                                self.output.write_all(b">\n")?;
                            } else {
                                writeln!(self.output, " start=\"{}\">", nl.start)?;
                            }
                        }
                    }
                } else if nl.list_type == ListType::Bullet {
                    self.output.write_all(b"</ul>\n")?;
                } else {
                    self.output.write_all(b"</ol>\n")?;
                }
            }
            NodeValue::Item(..) => {
                if entering {
                    self.cr()?;
                    self.output.write_all(b"<li")?;
                    self.render_sourcepos(node)?;
                    self.output.write_all(b">")?;
                } else {
                    self.output.write_all(b"</li>\n")?;
                }
            }
            NodeValue::DescriptionList => {
                if entering {
                    self.cr()?;
                    self.output.write_all(b"<dl")?;
                    self.render_sourcepos(node)?;
                    self.output.write_all(b">\n")?;
                } else {
                    self.output.write_all(b"</dl>\n")?;
                }
            }
            NodeValue::DescriptionItem(..) => (),
            NodeValue::DescriptionTerm => {
                if entering {
                    self.cr()?;
                    self.output.write_all(b"<dt")?;
                    self.render_sourcepos(node)?;
                    self.output.write_all(b">")?;
                } else {
                    self.output.write_all(b"</dt>\n")?;
                }
            }
            NodeValue::DescriptionDetails => {
                if entering {
                    self.cr()?;
                    self.output.write_all(b"<dd")?;
                    self.render_sourcepos(node)?;
                    self.output.write_all(b">")?;
                } else {
                    self.output.write_all(b"</dd>\n")?;
                }
            }
            NodeValue::Heading(ref nch) => match self.plugins.render.heading_adapter {
                None => {
                    if entering {
                        self.cr()?;
                        write!(self.output, "<h{}", nch.level)?;
                        if self.options.extension.header_ids.is_some() {
                            let mut text_content = Vec::with_capacity(20);
                            Self::collect_text(node, &mut text_content);

                            let raw_id = String::from_utf8(text_content).unwrap();
                            let is_templ = raw_id.contains(DELIM_START);
                            if is_templ {
                                write!(self.output, " data-update-id")?;
                            } else {
                                let id = self.anchorizer.anchorize(&raw_id);
                                write!(self.output, " id=\"{}\"", id)?;
                            };
                        }
                        self.render_sourcepos(node)?;
                        self.output.write_all(b">")?;
                    } else {
                        writeln!(self.output, "</h{}>", nch.level)?;
                    }
                }
                Some(adapter) => {
                    let mut text_content = Vec::with_capacity(20);
                    Self::collect_text(node, &mut text_content);
                    let content = String::from_utf8(text_content).unwrap();
                    let heading = HeadingMeta {
                        level: nch.level,
                        content,
                    };

                    if entering {
                        self.cr()?;
                        adapter.enter(
                            self.output,
                            &heading,
                            if self.options.render.sourcepos {
                                Some(node.data.borrow().sourcepos)
                            } else {
                                None
                            },
                        )?;
                    } else {
                        adapter.exit(self.output, &heading)?;
                    }
                }
            },
            NodeValue::CodeBlock(ref ncb) => {
                if entering {
                    if ncb.info.eq("math") {
                        self.render_math_code_block(node, &ncb.literal)?;
                    } else {
                        self.cr()?;

                        let mut first_tag = 0;
                        let mut pre_attributes: HashMap<String, String> = HashMap::new();
                        let mut code_attributes: HashMap<String, String> = HashMap::new();
                        let code_attr: String;

                        let literal = &ncb.literal.as_bytes();
                        let info = &ncb.info.as_bytes();

                        if !info.is_empty() {
                            while first_tag < info.len() && !isspace(info[first_tag]) {
                                first_tag += 1;
                            }

                            let lang_str = str::from_utf8(&info[..first_tag]).unwrap();
                            let info_str = str::from_utf8(&info[first_tag..]).unwrap().trim();

                            if self.options.render.github_pre_lang {
                                pre_attributes.insert(String::from("lang"), lang_str.to_string());

                                if self.options.render.full_info_string && !info_str.is_empty() {
                                    pre_attributes.insert(
                                        String::from("data-meta"),
                                        info_str.trim().to_string(),
                                    );
                                }
                            } else {
                                code_attr = format!("language-{}", lang_str);
                                code_attributes.insert(String::from("class"), code_attr);

                                if self.options.render.full_info_string && !info_str.is_empty() {
                                    code_attributes
                                        .insert(String::from("data-meta"), info_str.to_string());
                                }
                            }
                        }

                        if self.options.render.sourcepos {
                            let ast = node.data.borrow();
                            pre_attributes
                                .insert("data-sourcepos".to_string(), ast.sourcepos.to_string());
                        }

                        match self.plugins.render.codefence_syntax_highlighter {
                            None => {
                                pre_attributes.extend(code_attributes);
                                let _with_code = if let Some(cls) = pre_attributes.get_mut("class")
                                {
                                    if !ncb.info.is_empty() {
                                        let langs = ncb
                                            .info
                                            .split_ascii_whitespace()
                                            .map(|s| s.strip_suffix("-nolint").unwrap_or(s))
                                            .join(" ");

                                        *cls = format!("brush: {langs} notranslate",);
                                        &ncb.info != "plain"
                                    } else {
                                        *cls = "notranslate".to_string();
                                        false
                                    }
                                } else {
                                    pre_attributes.insert("class".into(), "notranslate".into());
                                    false
                                };
                                write_opening_tag(self.output, "pre", pre_attributes)?;
                                self.escape(literal)?;
                                self.output.write_all(b"</pre>\n")?
                            }
                            Some(highlighter) => {
                                highlighter.write_pre_tag(self.output, pre_attributes)?;
                                highlighter.write_code_tag(self.output, code_attributes)?;

                                highlighter.write_highlighted(
                                    self.output,
                                    str::from_utf8(&info[..first_tag]).ok(),
                                    &ncb.literal,
                                )?;

                                self.output.write_all(b"</code></pre>\n")?
                            }
                        }
                    }
                }
            }
            NodeValue::HtmlBlock(ref nhb) => {
                // No sourcepos.
                if entering {
                    let is_marco = nhb.literal.starts_with("<!-- ks____");
                    if !is_marco {
                        self.cr()?;
                    }
                    let literal = if is_marco {
                        nhb.literal
                            .strip_suffix('\n')
                            .unwrap_or(&nhb.literal)
                            .as_bytes()
                    } else {
                        nhb.literal.as_bytes()
                    };
                    if self.options.render.escape {
                        self.escape(literal)?;
                    } else if !self.options.render.unsafe_ {
                        self.output.write_all(b"<!-- raw HTML omitted -->")?;
                    } else if self.options.extension.tagfilter {
                        tagfilter_block(literal, &mut self.output)?;
                    } else {
                        self.output.write_all(literal)?;
                    }
                    if !is_marco {
                        self.cr()?;
                    }
                }
            }
            NodeValue::ThematicBreak => {
                if entering {
                    self.cr()?;
                    self.output.write_all(b"<hr")?;
                    self.render_sourcepos(node)?;
                    self.output.write_all(b" />\n")?;
                }
            }
            NodeValue::Paragraph => {
                let tight = match node
                    .parent()
                    .and_then(|n| n.parent())
                    .map(|n| n.data.borrow().value.clone())
                {
                    Some(NodeValue::List(nl)) => nl.tight,
                    Some(NodeValue::DescriptionItem(nd)) => nd.tight,
                    _ => false,
                };

                let tight = tight
                    || matches!(
                        node.parent().map(|n| n.data.borrow().value.clone()),
                        Some(NodeValue::DescriptionTerm)
                    );

                if !tight {
                    if entering {
                        self.cr()?;
                        self.output.write_all(b"<p")?;
                        self.render_sourcepos(node)?;
                        self.output.write_all(b">")?;
                    } else {
                        if let NodeValue::FootnoteDefinition(nfd) =
                            &node.parent().unwrap().data.borrow().value
                        {
                            if node.next_sibling().is_none() {
                                self.output.write_all(b" ")?;
                                self.put_footnote_backref(nfd)?;
                            }
                        }
                        self.output.write_all(b"</p>\n")?;
                    }
                }
            }
            NodeValue::Text(ref literal) => {
                // Nowhere to put sourcepos.
                if entering {
                    self.escape(literal.as_bytes())?;
                }
            }
            NodeValue::LineBreak => {
                // Unreliable sourcepos.
                if entering {
                    self.output.write_all(b"<br")?;
                    if self.options.render.experimental_inline_sourcepos {
                        self.render_sourcepos(node)?;
                    }
                    self.output.write_all(b" />\n")?;
                }
            }
            NodeValue::SoftBreak => {
                // Unreliable sourcepos.
                if entering {
                    if self.options.render.hardbreaks {
                        self.output.write_all(b"<br")?;
                        if self.options.render.experimental_inline_sourcepos {
                            self.render_sourcepos(node)?;
                        }
                        self.output.write_all(b" />\n")?;
                    } else {
                        self.output.write_all(b"\n")?;
                    }
                }
            }
            NodeValue::Code(NodeCode { ref literal, .. }) => {
                // Unreliable sourcepos.
                if entering {
                    self.output.write_all(b"<code")?;
                    if self.options.render.experimental_inline_sourcepos {
                        self.render_sourcepos(node)?;
                    }
                    self.output.write_all(b">")?;
                    self.escape(literal.as_bytes())?;
                    self.output.write_all(b"</code>")?;
                }
            }
            NodeValue::HtmlInline(ref literal) => {
                // No sourcepos.
                if entering {
                    let literal = literal.as_bytes();
                    if self.options.render.escape {
                        self.escape(literal)?;
                    } else if !self.options.render.unsafe_ {
                        self.output.write_all(b"<!-- raw HTML omitted -->")?;
                    } else if self.options.extension.tagfilter && tagfilter(literal) {
                        self.output.write_all(b"&lt;")?;
                        self.output.write_all(&literal[1..])?;
                    } else {
                        self.output.write_all(literal)?;
                    }
                }
            }
            NodeValue::Raw(ref literal) => {
                // No sourcepos.
                if entering {
                    self.output.write_all(literal.as_bytes())?;
                }
            }
            NodeValue::Strong => {
                // Unreliable sourcepos.
                let parent_node = node.parent();
                if !self.options.render.gfm_quirks
                    || (parent_node.is_none()
                        || !matches!(parent_node.unwrap().data.borrow().value, NodeValue::Strong))
                {
                    if entering {
                        self.output.write_all(b"<strong")?;
                        if self.options.render.experimental_inline_sourcepos {
                            self.render_sourcepos(node)?;
                        }
                        self.output.write_all(b">")?;
                    } else {
                        self.output.write_all(b"</strong>")?;
                    }
                }
            }
            NodeValue::Emph => {
                // Unreliable sourcepos.
                if entering {
                    self.output.write_all(b"<em")?;
                    if self.options.render.experimental_inline_sourcepos {
                        self.render_sourcepos(node)?;
                    }
                    self.output.write_all(b">")?;
                } else {
                    self.output.write_all(b"</em>")?;
                }
            }
            NodeValue::Strikethrough => {
                // Unreliable sourcepos.
                if entering {
                    self.output.write_all(b"<del")?;
                    if self.options.render.experimental_inline_sourcepos {
                        self.render_sourcepos(node)?;
                    }
                    self.output.write_all(b">")?;
                } else {
                    self.output.write_all(b"</del>")?;
                }
            }
            NodeValue::Superscript => {
                // Unreliable sourcepos.
                if entering {
                    self.output.write_all(b"<sup")?;
                    if self.options.render.experimental_inline_sourcepos {
                        self.render_sourcepos(node)?;
                    }
                    self.output.write_all(b">")?;
                } else {
                    self.output.write_all(b"</sup>")?;
                }
            }
            NodeValue::Link(ref nl) => {
                // Unreliable sourcepos.
                let parent_node = node.parent();

                if !self.options.parse.relaxed_autolinks
                    || (parent_node.is_none()
                        || !matches!(
                            parent_node.unwrap().data.borrow().value,
                            NodeValue::Link(..)
                        ))
                {
                    if entering {
                        self.output.write_all(b"<a")?;
                        if self.options.render.experimental_inline_sourcepos {
                            self.render_sourcepos(node)?;
                        }
                        self.output.write_all(b" href=\"")?;
                        let url = nl.url.as_bytes();
                        if self.options.render.unsafe_ || !dangerous_url(url) {
                            if let Some(rewriter) = &self.options.extension.link_url_rewriter {
                                self.escape_href(rewriter.to_html(&nl.url).as_bytes())?;
                            } else {
                                self.escape_href(url)?;
                            }
                        }
                        if !nl.title.is_empty() {
                            self.output.write_all(b"\" title=\"")?;
                            self.escape(nl.title.as_bytes())?;
                        }
                        let mut text_content = Vec::with_capacity(20);
                        Self::collect_text(node, &mut text_content);

                        if text_content == url {
                            self.output.write_all(b"\" data-autolink=\"")?;
                        }
                        self.output.write_all(b"\">")?;
                    } else {
                        self.output.write_all(b"</a>")?;
                    }
                }
            }

            NodeValue::Image(ref nl) => {
                // Unreliable sourcepos.
                if entering {
                    self.output.write_all(b"<img")?;
                    if self.options.render.experimental_inline_sourcepos {
                        self.render_sourcepos(node)?;
                    }
                    self.output.write_all(b" src=\"")?;
                    let url = nl.url.as_bytes();
                    if self.options.render.unsafe_ || !dangerous_url(url) {
                        if let Some(rewriter) = &self.options.extension.image_url_rewriter {
                            self.escape_href(rewriter.to_html(&nl.url).as_bytes())?;
                        } else {
                            self.escape_href(url)?;
                        }
                    }
                    self.output.write_all(b"\" alt=\"")?;
                    return Ok((true, Flag::None));
                } else {
                    if !nl.title.is_empty() {
                        self.output.write_all(b"\" title=\"")?;
                        self.escape(nl.title.as_bytes())?;
                    }
                    self.output.write_all(b"\" />")?;
                }
            }
            NodeValue::Table(..) => {
                if entering {
                    self.cr()?;
                    self.output.write_all(b"<table")?;
                    self.render_sourcepos(node)?;
                    self.output.write_all(b">\n")?;
                } else {
                    if !node
                        .last_child()
                        .unwrap()
                        .same_node(node.first_child().unwrap())
                    {
                        self.cr()?;
                        self.output.write_all(b"</tbody>\n")?;
                    }
                    self.cr()?;
                    self.output.write_all(b"</table>\n")?;
                }
            }
            NodeValue::TableRow(header) => {
                if entering {
                    self.cr()?;
                    if header {
                        self.output.write_all(b"<thead>\n")?;
                    } else if let Some(n) = node.previous_sibling() {
                        if let NodeValue::TableRow(true) = n.data.borrow().value {
                            self.output.write_all(b"<tbody>\n")?;
                        }
                    }
                    self.output.write_all(b"<tr")?;
                    self.render_sourcepos(node)?;
                    self.output.write_all(b">")?;
                } else {
                    self.cr()?;
                    self.output.write_all(b"</tr>")?;
                    if header {
                        self.cr()?;
                        self.output.write_all(b"</thead>")?;
                    }
                }
            }
            NodeValue::TableCell => {
                let row = &node.parent().unwrap().data.borrow().value;
                let in_header = match *row {
                    NodeValue::TableRow(header) => header,
                    _ => panic!(),
                };

                let table = &node.parent().unwrap().parent().unwrap().data.borrow().value;
                let alignments = match *table {
                    NodeValue::Table(NodeTable { ref alignments, .. }) => alignments,
                    _ => panic!(),
                };

                if entering {
                    self.cr()?;
                    if in_header {
                        self.output.write_all(b"<th")?;
                        self.render_sourcepos(node)?;
                    } else {
                        self.output.write_all(b"<td")?;
                        self.render_sourcepos(node)?;
                    }

                    let mut start = node.parent().unwrap().first_child().unwrap();
                    let mut i = 0;
                    while !start.same_node(node) {
                        i += 1;
                        start = start.next_sibling().unwrap();
                    }

                    match alignments[i] {
                        TableAlignment::Left => {
                            self.output.write_all(b" align=\"left\"")?;
                        }
                        TableAlignment::Right => {
                            self.output.write_all(b" align=\"right\"")?;
                        }
                        TableAlignment::Center => {
                            self.output.write_all(b" align=\"center\"")?;
                        }
                        TableAlignment::None => (),
                    }

                    self.output.write_all(b">")?;
                } else if in_header {
                    self.output.write_all(b"</th>")?;
                } else {
                    self.output.write_all(b"</td>")?;
                }
            }
            NodeValue::FootnoteDefinition(ref nfd) => {
                if entering {
                    if self.footnote_ix == 0 {
                        self.output.write_all(b"<section")?;
                        self.render_sourcepos(node)?;
                        self.output
                            .write_all(b" class=\"footnotes\" data-footnotes>\n<ol>\n")?;
                    }
                    self.footnote_ix += 1;
                    self.output.write_all(b"<li")?;
                    self.render_sourcepos(node)?;
                    self.output.write_all(b" id=\"fn-")?;
                    self.escape_href(nfd.name.as_bytes())?;
                    self.output.write_all(b"\">")?;
                } else {
                    if self.put_footnote_backref(nfd)? {
                        self.output.write_all(b"\n")?;
                    }
                    self.output.write_all(b"</li>\n")?;
                }
            }
            NodeValue::FootnoteReference(ref nfr) => {
                // Unreliable sourcepos.
                if entering {
                    let mut ref_id = format!("fnref-{}", nfr.name);
                    if nfr.ref_num > 1 {
                        ref_id = format!("{}-{}", ref_id, nfr.ref_num);
                    }

                    self.output.write_all(b"<sup")?;
                    if self.options.render.experimental_inline_sourcepos {
                        self.render_sourcepos(node)?;
                    }
                    self.output
                        .write_all(b" class=\"footnote-ref\"><a href=\"#fn-")?;
                    self.escape_href(nfr.name.as_bytes())?;
                    self.output.write_all(b"\" id=\"")?;
                    self.escape_href(ref_id.as_bytes())?;
                    write!(self.output, "\" data-footnote-ref>{}</a></sup>", nfr.ix)?;
                }
            }
            NodeValue::TaskItem(symbol) => {
                if entering {
                    self.cr()?;
                    self.output.write_all(b"<li")?;
                    if self.options.render.tasklist_classes {
                        self.output.write_all(b" class=\"task-list-item\"")?;
                    }
                    self.render_sourcepos(node)?;
                    self.output.write_all(b">")?;
                    self.output.write_all(b"<input type=\"checkbox\"")?;
                    if self.options.render.tasklist_classes {
                        self.output
                            .write_all(b" class=\"task-list-item-checkbox\"")?;
                    }
                    if symbol.is_some() {
                        self.output.write_all(b" checked=\"\"")?;
                    }
                    self.output.write_all(b" disabled=\"\" /> ")?;
                } else {
                    self.output.write_all(b"</li>\n")?;
                }
            }
            NodeValue::MultilineBlockQuote(_) => {
                if entering {
                    self.cr()?;
                    self.output.write_all(b"<blockquote")?;
                    self.render_sourcepos(node)?;
                    self.output.write_all(b">\n")?;
                } else {
                    self.cr()?;
                    self.output.write_all(b"</blockquote>\n")?;
                }
            }
            NodeValue::Escaped => {
                // Unreliable sourcepos.
                if self.options.render.escaped_char_spans {
                    if entering {
                        self.output.write_all(b"<span data-escaped-char")?;
                        if self.options.render.experimental_inline_sourcepos {
                            self.render_sourcepos(node)?;
                        }
                        self.output.write_all(b">")?;
                    } else {
                        self.output.write_all(b"</span>")?;
                    }
                }
            }
            NodeValue::Math(NodeMath {
                ref literal,
                display_math,
                dollar_math,
                ..
            }) => {
                if entering {
                    self.render_math_inline(node, literal, display_math, dollar_math)?;
                }
            }
            NodeValue::WikiLink(ref nl) => {
                // Unreliable sourcepos.
                if entering {
                    self.output.write_all(b"<a")?;
                    if self.options.render.experimental_inline_sourcepos {
                        self.render_sourcepos(node)?;
                    }
                    self.output.write_all(b" href=\"")?;
                    let url = nl.url.as_bytes();
                    if self.options.render.unsafe_ || !dangerous_url(url) {
                        self.escape_href(url)?;
                    }
                    self.output.write_all(b"\" data-wikilink=\"true")?;
                    self.output.write_all(b"\">")?;
                } else {
                    self.output.write_all(b"</a>")?;
                }
            }
            NodeValue::Underline => {
                // Unreliable sourcepos.
                if entering {
                    self.output.write_all(b"<u")?;
                    if self.options.render.experimental_inline_sourcepos {
                        self.render_sourcepos(node)?;
                    }
                    self.output.write_all(b">")?;
                } else {
                    self.output.write_all(b"</u>")?;
                }
            }
            NodeValue::Subscript => {
                // Unreliable sourcepos.
                if entering {
                    self.output.write_all(b"<sub")?;
                    if self.options.render.experimental_inline_sourcepos {
                        self.render_sourcepos(node)?;
                    }
                    self.output.write_all(b">")?;
                } else {
                    self.output.write_all(b"</sub>")?;
                }
            }
            NodeValue::SpoileredText => {
                // Unreliable sourcepos.
                if entering {
                    self.output.write_all(b"<span")?;
                    if self.options.render.experimental_inline_sourcepos {
                        self.render_sourcepos(node)?;
                    }
                    self.output.write_all(b" class=\"spoiler\">")?;
                } else {
                    self.output.write_all(b"</span>")?;
                }
            }
            NodeValue::EscapedTag(ref net) => {
                // Nowhere to put sourcepos.
                self.output.write_all(net.as_bytes())?;
            }
            NodeValue::Alert(ref alert) => {
                if entering {
                    self.cr()?;
                    self.output.write_all(b"<div class=\"markdown-alert ")?;
                    self.output
                        .write_all(alert_type_css_class(&alert.alert_type).as_bytes())?;
                    self.output.write_all(b"\"")?;
                    self.render_sourcepos(node)?;
                    self.output.write_all(b">\n")?;
                    self.output
                        .write_all(b"<p class=\"markdown-alert-title\">")?;
                    match alert.title {
                        Some(ref title) => self.escape(title.as_bytes())?,
                        None => {
                            self.output.write_all(
                                alert_type_default_title(&alert.alert_type).as_bytes(),
                            )?;
                        }
                    }
                    self.output.write_all(b"</p>\n")?;
                } else {
                    self.cr()?;
                    self.output.write_all(b"</div>\n")?;
                }
            }
        }
        Ok((false, Flag::None))
    }

    fn render_sourcepos<'a>(&mut self, node: &'a AstNode<'a>) -> io::Result<()> {
        if self.options.render.sourcepos {
            let ast = node.data.borrow();
            if ast.sourcepos.start.line > 0 {
                write!(self.output, " data-sourcepos=\"{}\"", ast.sourcepos)?;
            }
        }
        Ok(())
    }

    fn put_footnote_backref(&mut self, nfd: &NodeFootnoteDefinition) -> io::Result<bool> {
        if self.written_footnote_ix >= self.footnote_ix {
            return Ok(false);
        }

        self.written_footnote_ix = self.footnote_ix;

        let mut ref_suffix = String::new();
        let mut superscript = String::new();

        for ref_num in 1..=nfd.total_references {
            if ref_num > 1 {
                ref_suffix = format!("-{}", ref_num);
                superscript = format!("<sup class=\"footnote-ref\">{}</sup>", ref_num);
                write!(self.output, " ")?;
            }

            self.output.write_all(b"<a href=\"#fnref-")?;
            self.escape_href(nfd.name.as_bytes())?;
            write!(
                self.output,
                "{}\" class=\"footnote-backref\" data-footnote-backref data-footnote-backref-idx=\"{}{}\" aria-label=\"Back to reference {}{}\">↩{}</a>",
                ref_suffix, self.footnote_ix, ref_suffix, self.footnote_ix, ref_suffix, superscript
            )?;
        }
        Ok(true)
    }

    // Renders a math dollar inline, `$...$` and `$$...$$` using `<span>` to be similar
    // to other renderers.
    fn render_math_inline<'a>(
        &mut self,
        node: &'a AstNode<'a>,
        literal: &String,
        display_math: bool,
        dollar_math: bool,
    ) -> io::Result<()> {
        let mut tag_attributes: Vec<(String, String)> = Vec::new();
        let style_attr = if display_math { "display" } else { "inline" };
        let tag: &str = if dollar_math { "span" } else { "code" };

        tag_attributes.push((String::from("data-math-style"), String::from(style_attr)));

        // Unreliable sourcepos.
        if self.options.render.experimental_inline_sourcepos && self.options.render.sourcepos {
            let ast = node.data.borrow();
            tag_attributes.push(("data-sourcepos".to_string(), ast.sourcepos.to_string()));
        }

        write_opening_tag(self.output, tag, tag_attributes)?;
        self.escape(literal.as_bytes())?;
        write!(self.output, "</{}>", tag)?;

        Ok(())
    }

    // Renders a math code block, ```` ```math ```` using `<pre><code>`
    fn render_math_code_block<'a>(
        &mut self,
        node: &'a AstNode<'a>,
        literal: &String,
    ) -> io::Result<()> {
        self.cr()?;

        // use vectors to ensure attributes always written in the same order,
        // for testing stability
        let mut pre_attributes: Vec<(String, String)> = Vec::new();
        let mut code_attributes: Vec<(String, String)> = Vec::new();
        let lang_str = "math";

        if self.options.render.github_pre_lang {
            pre_attributes.push((String::from("lang"), lang_str.to_string()));
            pre_attributes.push((String::from("data-math-style"), String::from("display")));
        } else {
            let code_attr = format!("language-{}", lang_str);
            code_attributes.push((String::from("class"), code_attr));
            code_attributes.push((String::from("data-math-style"), String::from("display")));
        }

        if self.options.render.sourcepos {
            let ast = node.data.borrow();
            pre_attributes.push(("data-sourcepos".to_string(), ast.sourcepos.to_string()));
        }

        write_opening_tag(self.output, "pre", pre_attributes)?;
        write_opening_tag(self.output, "code", code_attributes)?;

        self.escape(literal.as_bytes())?;
        self.output.write_all(b"</code></pre>\n")?;

        Ok(())
    }
}
